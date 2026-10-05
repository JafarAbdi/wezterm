use crate::termwiztermtab;
use anyhow::{anyhow, bail, Context as _};
use crossbeam::channel::{unbounded, Receiver, Sender};
use finl_unicode::grapheme_clusters::Graphemes;
use promise::spawn::block_on;
use promise::{BrokenPromise, Promise};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use termwiz::cell::{unicode_column_width, CellAttributes};
use termwiz::lineedit::*;
use termwiz::surface::{Change, Position};
use termwiz::terminal::*;
use wezterm_term::TerminalSize;

#[derive(Default)]
struct PasswordPromptHost {
    history: BasicHistory,
}
impl LineEditorHost for PasswordPromptHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }

    // Rewrite the input so that we can obscure the password
    // characters when output to the terminal widget
    fn highlight_line(&self, line: &str, cursor_position: usize) -> (Vec<OutputElement>, usize) {
        let placeholder = "🔑";
        let grapheme_count = unicode_column_width(line, None);
        let mut output = vec![];
        for _ in 0..grapheme_count {
            output.push(OutputElement::Text(placeholder.to_string()));
        }
        (
            output,
            unicode_column_width(placeholder, None) * cursor_position,
        )
    }
}

/// The answer to one request.  Answering consumes it.  Dropping it
/// unanswered answers [`BrokenPromise`], because a dropped `Promise` never
/// wakes the asker that blocks on its future; that covers a consumer that
/// ends early and a request still queued when the last receiver goes.
pub struct Respond<T>(Option<Promise<T>>);

impl<T> Respond<T> {
    fn new() -> (Self, promise::Future<T>) {
        let mut promise = Promise::new();
        let future = promise.get_future().unwrap();
        (Self(Some(promise)), future)
    }

    pub fn result(mut self, result: anyhow::Result<T>) {
        if let Some(mut promise) = self.0.take() {
            promise.result(result);
        }
    }

    pub fn ok(self, value: T) {
        self.result(Ok(value));
    }
}

impl<T> Drop for Respond<T> {
    fn drop(&mut self) {
        if let Some(mut promise) = self.0.take() {
            promise.err(BrokenPromise {}.into());
        }
    }
}

pub enum UIRequest {
    /// Display something
    Output(Vec<Change>),
    /// Request input
    Input {
        prompt: String,
        echo: bool,
        respond: Respond<String>,
    },
    /// Sleep with a progress bar
    Sleep {
        reason: String,
        duration: Duration,
        respond: Respond<()>,
    },
    Close,
    /// Ask whether to trust a host whose key is not known yet.
    HostTrust {
        /// The text a terminal shows before its y/n prompt.
        message: String,
        /// `host:port` of the server.
        remote_address: String,
        fingerprint: String,
        respond: Respond<bool>,
    },
}

struct ConnectionUIImpl {
    term: termwiztermtab::TermWizTerminal,
    rx: Receiver<UIRequest>,
}

#[derive(PartialEq, Eq)]
enum CloseStatus {
    Explicit,
    Implicit,
}

impl ConnectionUIImpl {
    fn run(&mut self) -> anyhow::Result<CloseStatus> {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UIRequest::Close) => return Ok(CloseStatus::Explicit),
                Ok(UIRequest::Output(changes)) => self.term.render(&changes)?,
                Ok(UIRequest::Input {
                    prompt,
                    echo: true,
                    respond,
                }) => {
                    respond.result(self.input_prompt(&prompt));
                }
                Ok(UIRequest::Input {
                    prompt,
                    echo: false,
                    respond,
                }) => {
                    respond.result(self.password_prompt(&prompt));
                }
                Ok(UIRequest::Sleep {
                    reason,
                    duration,
                    respond,
                }) => {
                    respond.result(self.sleep(&reason, duration));
                }
                Ok(UIRequest::HostTrust {
                    message, respond, ..
                }) => {
                    let message = format!("{}\n", message).replace("\n", "\r\n");
                    self.term.render(&[Change::Text(message)])?;
                    let trusted = match self.input_prompt("Enter [y/n]> ") {
                        Ok(line) => matches!(line.as_ref(), "y" | "Y" | "yes" | "YES"),
                        Err(_) => false,
                    };
                    respond.ok(trusted);
                }
                Err(err) if err.is_timeout() => {}
                Err(err) => bail!("recv_timeout: {}", err),
            }
        }
    }

    fn password_prompt(&mut self, prompt: &str) -> anyhow::Result<String> {
        let mut editor = LineEditor::new(&mut self.term);
        editor.set_prompt(prompt);

        let mut host = PasswordPromptHost::default();
        if let Some(line) = editor.read_line(&mut host)? {
            Ok(line)
        } else {
            bail!("password entry was cancelled");
        }
    }

    fn input_prompt(&mut self, prompt: &str) -> anyhow::Result<String> {
        let mut editor = LineEditor::new(&mut self.term);
        editor.set_prompt(prompt);

        let mut host = NopLineEditorHost::default();
        if let Some(line) = editor.read_line(&mut host)? {
            Ok(line)
        } else {
            bail!("prompt cancelled");
        }
    }

    fn sleep(&mut self, reason: &str, duration: Duration) -> anyhow::Result<()> {
        let start = Instant::now();
        let deadline = start + duration;
        let mut last_draw = None;

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }

            // Render a progress bar underneath the countdown text by reversing
            // out the text for the elapsed portion of time.
            let remain = deadline - now;
            let term_width = self.term.get_screen_size().map(|s| s.cols).unwrap_or(80);
            let prog_width = term_width as u128 * (duration.as_millis() - remain.as_millis())
                / duration.as_millis();
            let prog_width = prog_width as usize;
            let message = format!("{} ({:.0?})", reason, remain);

            let mut reversed_string = String::new();
            let mut default_string = String::new();
            let mut col = 0;
            for grapheme in Graphemes::new(&message) {
                // Once we've passed the elapsed column, full up the string
                // that we'll render with default attributes instead.
                if col > prog_width {
                    default_string.push_str(grapheme);
                } else {
                    reversed_string.push_str(grapheme);
                }
                col += 1;
            }

            // If we didn't reach the elapsed column yet (really short text!),
            // we need to pad out the reversed string.
            while col < prog_width {
                reversed_string.push(' ');
                col += 1;
            }

            let combined = format!("{}{}", reversed_string, default_string);

            if last_draw.is_none() || last_draw.as_ref().unwrap() != &combined {
                self.term.render(&[
                    Change::CursorPosition {
                        x: Position::Absolute(0),
                        y: Position::Relative(0),
                    },
                    Change::AllAttributes(CellAttributes::default().set_reverse(true).clone()),
                    Change::Text(reversed_string),
                    Change::AllAttributes(CellAttributes::default()),
                    Change::Text(default_string),
                ])?;
                last_draw.replace(combined);
            }

            // We use poll_input rather than a raw sleep here so that
            // eg: resize events can be processed and reflected in the
            // dimensions reported at the top of the loop.
            // We're using a sub-second value for the delay here for a
            // slightly smoother progress bar.
            self.term
                .poll_input(Some(remain.min(Duration::from_millis(50))))?;
        }

        let message = format!("{} (done)\r\n", reason);
        self.term.render(&[
            Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Relative(0),
            },
            Change::Text(message),
        ])?;

        Ok(())
    }
}

struct HeadlessImpl {
    rx: Receiver<UIRequest>,
}

impl HeadlessImpl {
    fn run(&mut self) -> anyhow::Result<()> {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UIRequest::Close) => break,
                Ok(UIRequest::Output(changes)) => {
                    log::trace!("Output: {:?}", changes);
                }
                Ok(UIRequest::Input { respond, .. }) => {
                    respond.result(Err(anyhow!("Input requested from headless context")));
                }
                Ok(UIRequest::Sleep {
                    respond,
                    reason,
                    duration,
                }) => {
                    log::error!("{} (sleeping for {:?})", reason, duration);
                    std::thread::sleep(duration);
                    respond.result(Ok(()));
                }
                Ok(UIRequest::HostTrust { respond, .. }) => {
                    respond.ok(false);
                }
                Err(err) if err.is_timeout() => {}
                Err(err) => bail!("recv_timeout: {}", err),
            }
        }

        Ok(())
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct ConnectionUIParams {
    pub size: TerminalSize,
    pub disable_close_delay: bool,
    pub window_id: Option<crate::WindowId>,
}

#[derive(Clone)]
pub struct ConnectionUI {
    tx: Sender<UIRequest>,
    /// The attempt this UI serves, when the caller can cancel it.
    cancel: Option<wezterm_ssh::Cancel>,
}

impl ConnectionUI {
    pub fn new() -> Self {
        Self::with_params(Default::default())
    }

    pub fn with_params(params: ConnectionUIParams) -> Self {
        let (tx, rx) = unbounded();
        promise::spawn::spawn_into_main_thread(termwiztermtab::run(
            params.size,
            params.window_id,
            move |term| {
                let mut ui = ConnectionUIImpl { term, rx };
                let status = ui.run().unwrap_or_else(|e| {
                    log::error!("while running ConnectionUI loop: {:?}", e);
                    CloseStatus::Implicit
                });

                if !params.disable_close_delay && status == CloseStatus::Implicit {
                    ui.sleep(
                        "(this window will close automatically)",
                        Duration::new(120, 0),
                    )
                    .ok();
                }
                Ok(())
            },
            None,
        ))
        .detach();
        Self { tx, cancel: None }
    }

    pub fn new_with_no_close_delay() -> Self {
        Self::with_params(ConnectionUIParams {
            disable_close_delay: true,
            ..Default::default()
        })
    }

    pub fn new_headless() -> Self {
        let (tx, rx) = unbounded();
        std::thread::spawn(move || {
            let mut ui = HeadlessImpl { rx };
            ui.run()
        });
        Self { tx, cancel: None }
    }

    /// A UI whose requests the caller consumes, in order, until every
    /// clone of the UI is dropped.  The consumer owns each `respond` from
    /// the moment it takes the request; a request it drops, or that is
    /// still queued when it drops the receiver, answers its asker with
    /// [`BrokenPromise`].  `cancel` cancels the
    /// attempt: the threads it starts count as its workers and nothing is
    /// published once it was cancelled.
    pub fn with_consumer(cancel: wezterm_ssh::Cancel) -> (Self, Receiver<UIRequest>) {
        let (tx, rx) = unbounded();
        (
            Self {
                tx,
                cancel: Some(cancel),
            },
            rx,
        )
    }

    /// The cancellation of the attempt this UI serves, if it has one.
    pub fn cancel(&self) -> Option<&wezterm_ssh::Cancel> {
        self.cancel.as_ref()
    }

    /// Count a thread that serves this UI's attempt until the guard drops.
    pub fn worker(&self) -> Option<wezterm_ssh::Worker> {
        self.cancel.as_ref().map(wezterm_ssh::Cancel::worker)
    }

    /// Ask the user whether to trust a host whose key is not known.  A UI
    /// that cannot ask does not trust.
    pub fn confirm_host_trust(
        &self,
        message: &str,
        remote_address: &str,
        fingerprint: &str,
    ) -> bool {
        let (respond, future) = Respond::new();
        let sent = self.tx.send(UIRequest::HostTrust {
            message: message.to_string(),
            remote_address: remote_address.to_string(),
            fingerprint: fingerprint.to_string(),
            respond,
        });
        sent.is_ok() && block_on(future).unwrap_or(false)
    }

    pub fn run_and_log_error<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> anyhow::Result<T>,
    {
        match f() {
            Err(e) => {
                let what = format!("\r\nFailed: {:?}\r\n", e);
                log::error!("{}", what);
                self.output_str(&what);
                Err(e)
            }
            result => result,
        }
    }

    pub async fn async_run_and_log_error<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: std::future::Future<Output = anyhow::Result<T>>,
    {
        match f.await {
            Err(e) => {
                let what = format!("\r\nFailed: {:?}\r\n", e);
                self.output_str(&what);
                Err(e)
            }
            result => result,
        }
    }

    pub fn title(&self, title: &str) {
        self.output(vec![Change::Title(title.to_string())]);
    }

    pub fn output(&self, changes: Vec<Change>) {
        self.tx.send(UIRequest::Output(changes)).ok();
    }

    pub fn output_str(&self, s: &str) {
        let s = s.replace("\n", "\r\n");
        self.output(vec![Change::Text(s)]);
    }

    /// Sleep (blocking!) for the specified duration, but updates
    /// the UI with the reason and a count down during that time.
    pub fn sleep_with_reason(&self, reason: &str, duration: Duration) -> anyhow::Result<()> {
        let (respond, future) = Respond::new();

        self.tx
            .send(UIRequest::Sleep {
                reason: reason.to_string(),
                duration,
                respond,
            })
            .context("send to ConnectionUI failed")?;

        block_on(future)
    }

    /// Crack a multi-line prompt into an optional preamble and the prompt
    /// text on the final line.  This is needed because the line editor
    /// is only designed for a single line prompt; a multi-line prompt
    /// messes up the cursor positioning.
    fn split_multi_line_prompt(s: &str) -> (Option<String>, String) {
        let text = s.replace("\n", "\r\n");
        let bits: Vec<&str> = text.rsplitn(2, "\r\n").collect();

        if bits.len() == 2 {
            (Some(format!("{}\r\n", bits[1])), bits[0].to_owned())
        } else {
            (None, text)
        }
    }

    pub fn input(&self, prompt: &str) -> anyhow::Result<String> {
        let (respond, future) = Respond::new();

        let (preamble, prompt) = Self::split_multi_line_prompt(prompt);
        if let Some(preamble) = preamble {
            self.output(vec![Change::Text(preamble)]);
        }

        self.tx
            .send(UIRequest::Input {
                prompt,
                echo: true,
                respond,
            })
            .context("send to ConnectionUI failed")?;

        block_on(future)
    }

    pub fn password(&self, prompt: &str) -> anyhow::Result<String> {
        let (respond, future) = Respond::new();

        let (preamble, prompt) = Self::split_multi_line_prompt(prompt);
        if let Some(preamble) = preamble {
            self.output(vec![Change::Text(preamble)]);
        }

        self.tx
            .send(UIRequest::Input {
                prompt,
                echo: false,
                respond,
            })
            .context("send to ConnectionUI failed")?;

        block_on(future)
    }

    pub fn close(&self) {
        self.tx.send(UIRequest::Close).ok();
    }

    pub fn test_alive(&self) -> bool {
        if !self.tx.send(UIRequest::Output(vec![])).is_ok() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
        self.tx.send(UIRequest::Output(vec![])).is_ok()
    }
}

lazy_static::lazy_static! {
    static ref ERROR_WINDOW: Mutex<Option<ConnectionUI>> = Mutex::new(None);
}

fn get_error_window() -> ConnectionUI {
    let mut err = ERROR_WINDOW.lock().unwrap();
    if let Some(ui) = err.as_ref().map(|ui| ui.clone()) {
        ui.output_str("\n");
        if ui.test_alive() {
            return ui;
        }
    }

    let ui = ConnectionUI::new_with_no_close_delay();
    ui.title("wezterm Configuration Error");
    err.replace(ui.clone());
    ui
}

/// If the GUI has been started, pops up a window with the supplied error
/// message framed as a configuration error.
/// If there is no GUI front end, generates a toast notification instead.
pub fn show_configuration_error_message(err: &str) {
    log::error!("Configuration Error: {}", err);
    let ui = get_error_window();

    let mut wrapped = textwrap::fill(&err, 78);
    wrapped.push_str("\n");
    ui.output_str(&wrapped);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam::channel::Select;

    fn consumer() -> (ConnectionUI, Receiver<UIRequest>) {
        ConnectionUI::with_consumer(wezterm_ssh::Cancel::new(|| {}).unwrap())
    }

    /// Block until a request is queued, without taking it.
    fn await_queued(requests: &Receiver<UIRequest>) {
        let mut select = Select::new();
        select.recv(requests);
        select.ready();
    }

    #[test]
    fn a_request_the_consumer_drops_unanswered_resumes_its_asker() {
        let (ui, requests) = consumer();
        let asker =
            std::thread::spawn(move || ui.password("Password: ").map_err(|e| e.to_string()));
        await_queued(&requests);
        drop(requests);
        assert_eq!(
            asker.join().unwrap(),
            Err("Promise was dropped before completion".to_string()),
            "queued when the consumer ended"
        );

        let (ui, requests) = consumer();
        let asker =
            std::thread::spawn(move || ui.confirm_host_trust("message", "host:22", "SHA256:x"));
        let request = requests.recv().unwrap();
        assert!(matches!(request, UIRequest::HostTrust { .. }));
        drop(request);
        assert!(
            !asker.join().unwrap(),
            "taken, then dropped by the consumer"
        );
    }

    #[test]
    fn a_request_after_the_consumer_ended_fails_without_waiting() {
        let (ui, requests) = consumer();
        drop(requests);
        assert_eq!(
            ui.password("Password: ").unwrap_err().to_string(),
            "send to ConnectionUI failed"
        );
        assert!(!ui.confirm_host_trust("message", "host:22", "SHA256:x"));
    }
}
