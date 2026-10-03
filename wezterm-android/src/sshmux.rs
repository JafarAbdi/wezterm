//! Attaching to the laptop mux: the one SSHMUX `ClientDomain`.
//!
//! [`connect`] validates the profile, starts the only connect operation
//! and returns.  The attach itself is the desktop one
//! (`ClientDomain::attach_with_ui`): native SSH, native `known_hosts`
//! verification, the codec version check, then the pane list.  Its
//! `ConnectionUI` requests are consumed here and become state in
//! [`Connections`]; Kotlin shows that state and answers prompts by id.
//!
//! Each attempt registers one domain, `laptop-<attempt>`, and no other
//! domain is ever added to the mux.  Nothing is spawned when the laptop has
//! no panes, and the proxy command never starts a server.  Once the
//! threads of an attempt have ended, its domain is detached and
//! unregistered on the GUI thread; an attached connection keeps it, also
//! while the laptop has no panes.
//!
//! [`cancel`] ends an attempt from the calling thread, without a GUI task:
//! its prompt ends, its transport shuts down, and the attach can no longer
//! publish.  [`disconnect`] detaches the domain on the GUI thread, so no
//! pane removal asks the laptop to kill a pane, and only then shuts the
//! transport down.  A reconnect is a new attempt: a new domain, a new
//! client and a new pane list.  Nothing reconnects by itself.

#![forbid(unsafe_code)]

use crate::connection::{
    Answer, AnswerRefused, Connections, EndRefused, Failure, FailureKind, PromptKind, Refused,
    Responder, Snapshot,
};
use crate::engine::EngineState;
use crate::profile::{Profile, ProfileError, ProfileFields};
use crate::sshstore::{ImportError, SshStore};
use config::SshDomain;
use crossbeam::channel::{Receiver, Sender};
use mux::connui::{ConnectionUI, UIRequest};
use mux::domain::{Domain, DomainId, DomainState};
use mux::ssh::{SshConnectError, SshConnectStage};
use mux::{Mux, MuxNotification};
use serde::Serialize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;
use termwiz::surface::Change;
use thiserror::Error;
use wezterm_client::client::{IncompatibleVersionError, VersionCheckFailed};
use wezterm_client::domain::{ClientDomain, ClientDomainConfig};
use window::os::android::platform_requests;

static CONNECTION: Connections =
    Connections::new(|| platform_requests().connection_changed(), retire);
static STORE: OnceLock<SshStore> = OnceLock::new();

/// A channel that disconnects when the engine ends.  An attach task that
/// the ended GUI thread never finishes keeps its `ConnectionUI`, so the
/// request channel of its consumer never closes; the consumer stops on
/// this instead.
struct EngineLive {
    live: Mutex<Option<Sender<()>>>,
    ended: Receiver<()>,
}

static ENGINE_LIVE: LazyLock<EngineLive> = LazyLock::new(|| {
    let (live, ended) = crossbeam::channel::bounded(0);
    EngineLive {
        live: Mutex::new(Some(live)),
        ended,
    }
});

/// Create the private SSH directory under `files_dir`, once per process.
pub(crate) fn open_store(files_dir: &Path) {
    if STORE.get().is_some() {
        return;
    }
    match SshStore::create(files_dir) {
        Ok(store) => {
            STORE.set(store).ok();
        }
        Err(err) => log::error!("private SSH directory: {err}"),
    }
}

/// The GUI engine ended with `message`.
pub(crate) fn engine_ended(message: &str) {
    CONNECTION.engine_ended(message);
    ENGINE_LIVE.live.lock().unwrap().take();
}

/// Why [`connect`] started nothing.
#[derive(Debug, Error)]
pub enum ConnectRefused {
    /// A profile field is invalid.
    #[error(transparent)]
    Profile(#[from] ProfileError),
    /// A connection is under way, or the last one is still closing.
    #[error(transparent)]
    Refused(#[from] Refused),
    /// The GUI engine has not started running yet.
    #[error("the terminal engine is still starting")]
    Starting,
    /// The private SSH directory could not be created.
    #[error("the app's private SSH directory is unavailable")]
    Storage,
}

/// Start attaching to the laptop `fields` name; returns the attempt id.
pub fn connect(fields: &ProfileFields) -> Result<u64, ConnectRefused> {
    let profile = Profile::parse(fields)?;
    let store = STORE.get().ok_or(ConnectRefused::Storage)?;
    if matches!(
        crate::terminal::engine_state(),
        EngineState::NotStarted | EngineState::Starting { .. }
    ) {
        return Err(ConnectRefused::Starting);
    }
    // `_starter` counts this call until the threads below own the attempt.
    let (attempt, cancel, _starter) = CONNECTION.begin()?;
    // The engine publishes its end to `CONNECTION` after it stops accepting
    // work, so an attempt begun before that is failed by it and one begun
    // after sees the ended state here.
    if !matches!(crate::terminal::engine_state(), EngineState::Running { .. }) {
        CONNECTION.finish(
            attempt,
            Err(Failure {
                kind: FailureKind::EngineEnded,
                message: "the GUI engine is not running".to_string(),
            }),
        );
        return Ok(attempt);
    }
    let domain = profile.ssh_domain(
        &domain_name(attempt),
        &store.known_hosts(),
        store.identity().as_deref(),
    );
    let (ui, requests) = ConnectionUI::with_consumer(cancel.clone());
    let worker = cancel.worker();
    let ended = ENGINE_LIVE.ended.clone();
    let consumer = std::thread::Builder::new()
        .name("wezterm-connect-ui".into())
        .spawn(move || {
            let _worker = worker;
            consume(attempt, &requests, &ended)
        });
    if let Err(err) = consumer {
        CONNECTION.finish(
            attempt,
            Err(Failure {
                kind: FailureKind::Other,
                message: format!("spawn connection UI thread: {err}"),
            }),
        );
        return Ok(attempt);
    }
    promise::spawn::spawn_into_main_thread(async move {
        let outcome = attach(attempt, domain, ui)
            .await
            .map_err(|err| classify(&err));
        if let Err(failure) = &outcome {
            log::error!("attach attempt {attempt} ended: {:?}", failure.kind);
        }
        CONNECTION.finish(attempt, outcome);
    })
    .detach();
    Ok(attempt)
}

fn domain_name(attempt: u64) -> String {
    format!("laptop-{attempt}")
}

/// GUI thread: register the domain, attach, and keep the window count in
/// step with the mux.  Returns the number of mux windows.
async fn attach(attempt: u64, ssh: SshDomain, ui: ConnectionUI) -> anyhow::Result<usize> {
    let mux = Mux::get();
    let domain: Arc<dyn Domain> = Arc::new(ClientDomain::new(ClientDomainConfig::Ssh(ssh)));
    mux.add_domain(&domain);
    CONNECTION.registered(attempt, domain.domain_id());
    let client = domain
        .downcast_ref::<ClientDomain>()
        .expect("the domain was created as a ClientDomain");
    client.attach_with_ui(None, ui).await?;
    // New-session actions resolve the default domain; keep it the laptop.
    mux.set_default_domain(&domain);

    let domain_id = domain.domain_id();
    let live = Arc::new(AtomicBool::new(true));
    mux.subscribe(move |notification| {
        if matches!(
            notification,
            MuxNotification::WindowCreated(_)
                | MuxNotification::WindowRemoved(_)
                | MuxNotification::PaneRemoved(_)
                | MuxNotification::Empty
        ) {
            // The mux may hold its own locks while it notifies.
            let live = Arc::clone(&live);
            promise::spawn::spawn_into_main_thread(async move {
                let Some(mux) = Mux::try_get() else { return };
                let attached = mux
                    .get_domain(domain_id)
                    .is_some_and(|domain| domain.state() == DomainState::Attached);
                if attached {
                    CONNECTION.windows(attempt, mux.iter_windows().len());
                } else {
                    live.store(false, Ordering::SeqCst);
                }
            })
            .detach();
        }
        live.load(Ordering::SeqCst)
    });
    Ok(mux.iter_windows().len())
}

/// Cancel `attempt` while it attaches; refused once it published its panes.
pub fn cancel(attempt: u64) -> Result<(), EndRefused> {
    CONNECTION.cancel_attempt(attempt)
}

/// Disconnect the attached `attempt`.  The laptop's panes are not touched:
/// the domain is detached before the transport goes down, and a detached
/// domain's panes are removed without asking the laptop to kill them.
pub fn disconnect(attempt: u64) -> Result<(), EndRefused> {
    let transport = CONNECTION.disconnect(attempt)?;
    // If the engine ends before this runs, its end shuts the transport down.
    promise::spawn::spawn_into_main_thread(async move {
        if let Some(domain) = Mux::get().get_domain_by_name(&domain_name(attempt))
            && let Some(client) = domain.downcast_ref::<ClientDomain>()
        {
            client.perform_detach();
        }
        transport.shutdown();
    })
    .detach();
    Ok(())
}

/// The threads of `attempt` have ended: unregister its domain `id` on the
/// GUI thread.  Detaching first removes the domain's panes while the
/// domain is still registered and detached, so no `ClientPane::kill` asks
/// the laptop to kill a pane; it also covers the client thread's own
/// detach task, which finds no domain once this ran.
fn retire(attempt: u64, id: DomainId) {
    promise::spawn::spawn_into_main_thread(async move {
        if let Some(mux) = Mux::try_get()
            && let Some(domain) = mux.get_domain(id)
        {
            if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                client.perform_detach();
            }
            mux.remove_domain(id);
        }
        CONNECTION.retired(attempt);
    })
    .detach();
}

/// Armed by [`hold_next_input`].
#[cfg(debug_assertions)]
static HOLD_NEXT_INPUT: AtomicBool = AtomicBool::new(false);

/// The progress line of an attempt whose input request is held.
#[cfg(debug_assertions)]
const HELD_INPUT: &str = "debug: an input request is held until the engine ends";

/// Make the consumer of an attempt take its next input request and then
/// wait for the engine's end and stop without answering or registering it,
/// as when its `select!` takes the end while that request is queued.  The
/// attempt shows `HELD_INPUT` as its progress meanwhile.  Debug evidence
/// only.
#[cfg(debug_assertions)]
pub fn hold_next_input() {
    HOLD_NEXT_INPUT.store(true, Ordering::SeqCst);
}

/// Shut the transport of the latest attempt down as a failing network
/// would, whatever its phase.  Debug evidence only.
#[cfg(debug_assertions)]
pub fn interrupt_transport() -> bool {
    CONNECTION.interrupt_transport()
}

fn classify(err: &anyhow::Error) -> Failure {
    let kind = if let Some(ssh) = err.downcast_ref::<SshConnectError>() {
        match ssh {
            SshConnectError::HostKeyChanged => FailureKind::HostKeyChanged,
            SshConnectError::AuthenticationCancelled => FailureKind::AuthenticationCancelled,
            SshConnectError::Session { stage, .. } => match stage {
                SshConnectStage::Connect => FailureKind::Unreachable,
                SshConnectStage::HostTrustDeclined => FailureKind::HostKeyRejected,
                SshConnectStage::HostKeyTypeChanged => FailureKind::HostKeyChanged,
                SshConnectStage::Authenticate => FailureKind::Authentication,
            },
        }
    } else if err.is::<IncompatibleVersionError>() {
        FailureKind::IncompatibleVersion
    } else if err.is::<VersionCheckFailed>() {
        FailureKind::ServerUnavailable
    } else {
        FailureKind::Other
    };
    Failure {
        kind,
        message: format!("{err:#}"),
    }
}

/// Own every request of one attempt's `ConnectionUI` until the attach
/// drops its last clone or the engine ends.  A prompt's answer moves into
/// [`Connections`] before the platform can see the prompt; a request still
/// queued when the engine ends is answered as it is dropped with the
/// receiver.
fn consume(attempt: u64, requests: &Receiver<UIRequest>, ended: &Receiver<()>) {
    loop {
        let request = crossbeam::select! {
            recv(requests) -> request => match request {
                Ok(request) => request,
                Err(_) => return,
            },
            recv(ended) -> _ => return,
        };
        #[cfg(debug_assertions)]
        if matches!(request, UIRequest::Input { .. })
            && HOLD_NEXT_INPUT.swap(false, Ordering::SeqCst)
        {
            CONNECTION.progress(attempt, HELD_INPUT);
            ended.recv().ok();
            return;
        }
        match request {
            UIRequest::Output(changes) => {
                for change in changes {
                    let Change::Text(text) = change else { continue };
                    if let Some(line) = text.lines().rev().find(|line| !line.trim().is_empty()) {
                        CONNECTION.progress(attempt, line.trim());
                    }
                }
            }
            UIRequest::Input {
                prompt,
                echo,
                respond,
            } => {
                let kind = if echo {
                    PromptKind::Text { text: prompt }
                } else {
                    PromptKind::Secret { text: prompt }
                };
                let responder: Responder = Box::new(move |outcome| {
                    respond.result(match outcome {
                        Ok(Answer::Text(text)) => Ok(text),
                        Ok(Answer::Trust(_)) => Err(AnswerRefused::WrongKind.into()),
                        Err(ended) => Err(ended.into()),
                    });
                });
                CONNECTION.ask(attempt, kind, responder);
            }
            UIRequest::HostTrust {
                remote_address,
                fingerprint,
                respond,
                ..
            } => {
                let kind = PromptKind::HostTrust {
                    remote_address,
                    fingerprint,
                };
                let responder: Responder = Box::new(move |outcome| {
                    respond.ok(outcome == Ok(Answer::Trust(true)));
                });
                CONNECTION.ask(attempt, kind, responder);
            }
            UIRequest::Sleep { respond, .. } => {
                // Only reconnect loops sleep, and SSHMUX has none.
                respond.result(Err(anyhow::anyhow!(
                    "the Android connection UI does not wait"
                )));
            }
            UIRequest::Close => {}
        }
    }
}

/// Answer the host-trust prompt `prompt` of `attempt`.
pub fn answer_host_trust(attempt: u64, prompt: u64, trust: bool) -> Result<(), AnswerRefused> {
    CONNECTION.answer(attempt, prompt, Answer::Trust(trust))
}

/// Answer the secret or text prompt `prompt` of `attempt`; `None` cancels
/// it.
pub fn answer_text(attempt: u64, prompt: u64, text: Option<String>) -> Result<(), AnswerRefused> {
    match text {
        Some(text) => CONNECTION.answer(attempt, prompt, Answer::Text(text)),
        None => CONNECTION.cancel(attempt, prompt),
    }
}

/// Connection state for the platform.
#[derive(Debug, Serialize)]
pub struct Status {
    /// Phase, prompt and revision.
    #[serde(flatten)]
    pub connection: Snapshot,
    /// Whether an identity was imported.
    pub identity: bool,
    /// Whether the GUI engine runs, so that [`connect`] can start an
    /// attempt.  The platform hears `ConnectionChanged` when it starts.
    pub ready: bool,
}

/// Snapshot of the connection state.
pub fn status() -> Status {
    Status {
        connection: CONNECTION.snapshot(),
        identity: STORE.get().is_some_and(|store| store.identity().is_some()),
        ready: matches!(crate::terminal::engine_state(), EngineState::Running { .. }),
    }
}

/// Block until the connection revision differs from `since`; returns the
/// revision then current.
pub fn await_change(since: i64, timeout_ms: i64) -> i64 {
    CONNECTION.await_change(
        u64::try_from(since).unwrap_or(0),
        Duration::from_millis(timeout_ms.max(0) as u64),
    ) as i64
}

/// Store the private key in `bytes` as the identity.
pub fn import_identity(bytes: &[u8]) -> Result<(), ImportError> {
    let store = STORE
        .get()
        .ok_or_else(|| std::io::Error::other("the private SSH directory is unavailable"))?;
    store.import_identity(bytes)?;
    Ok(())
}

/// Threads and open descriptors of this process.  Debug evidence only.
#[cfg(debug_assertions)]
#[derive(Debug, Serialize)]
pub struct ProcessCensus {
    /// Every thread of `/proc/self/task`: id and name (`comm`).
    pub threads: std::collections::BTreeMap<u32, String>,
    /// Every descriptor of `/proc/self/fd` and what it refers to, the
    /// census's own directory descriptor excluded.
    pub fds: std::collections::BTreeMap<u32, String>,
    /// Threads of the latest attempt that have not ended.
    pub workers: usize,
}

/// Take the census on the calling thread.
#[cfg(debug_assertions)]
pub fn process_census() -> ProcessCensus {
    fn entries(dir: &str) -> Vec<(u32, std::path::PathBuf)> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| Some((entry.file_name().to_str()?.parse().ok()?, entry.path())))
            .collect()
    }
    let threads = entries("/proc/self/task")
        .into_iter()
        .map(|(tid, path)| {
            let name = std::fs::read_to_string(path.join("comm")).unwrap_or_default();
            (tid, name.trim_end().to_string())
        })
        .collect();
    // The listing's own descriptor no longer resolves once it is closed.
    let fds = entries("/proc/self/fd")
        .into_iter()
        .filter_map(|(fd, path)| Some((fd, std::fs::read_link(path).ok()?.display().to_string())))
        .collect();
    ProcessCensus {
        threads,
        fds,
        workers: CONNECTION.snapshot().workers,
    }
}

/// The mux as the GUI thread sees it: every domain and every pane with the
/// ids the laptop knows them by.  Debug evidence only.
#[cfg(debug_assertions)]
#[derive(Debug, Serialize)]
pub struct MuxCensus {
    /// Every domain registered with the mux.
    pub domains: Vec<DomainEntry>,
    /// Every pane in the mux.
    pub panes: Vec<PaneEntry>,
}

/// One mux domain.
#[cfg(debug_assertions)]
#[derive(Debug, Serialize)]
pub struct DomainEntry {
    /// Its name.
    pub name: String,
    /// Whether it is a `ClientDomain`.
    pub client: bool,
    /// Whether it is attached.
    pub attached: bool,
}

/// One pane: phone-local ids and, for a client pane, the laptop's ids.
#[cfg(debug_assertions)]
#[derive(Debug, Serialize)]
pub struct PaneEntry {
    /// Local mux window.
    pub window: usize,
    /// Local tab.
    pub tab: usize,
    /// Local pane.
    pub pane: usize,
    /// The laptop's window id; `None` for a pane that is not remote.
    pub remote_window: Option<usize>,
    /// The laptop's tab id.
    pub remote_tab: Option<usize>,
    /// The laptop's pane id.
    pub remote_pane: Option<usize>,
    /// Whether the pane is a `ClientPane`.
    pub client: bool,
}

/// GUI thread: take the census.
#[cfg(debug_assertions)]
pub(crate) fn census() -> MuxCensus {
    use wezterm_client::pane::ClientPane;
    let mux = Mux::get();
    let domains = mux
        .iter_domains()
        .into_iter()
        .map(|domain| DomainEntry {
            name: domain.domain_name().to_string(),
            client: domain.downcast_ref::<ClientDomain>().is_some(),
            attached: domain.state() == DomainState::Attached,
        })
        .collect();
    let mut panes = Vec::new();
    for window_id in mux.iter_windows() {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        for tab in window.iter_tabs() {
            for positioned in tab.iter_panes_ignoring_zoom() {
                let pane = positioned.pane;
                let remote = pane.downcast_ref::<ClientPane>();
                let domain = mux.get_domain(pane.domain_id());
                let client_domain = domain
                    .as_ref()
                    .and_then(|domain| domain.downcast_ref::<ClientDomain>());
                panes.push(PaneEntry {
                    window: window_id,
                    tab: tab.tab_id(),
                    pane: pane.pane_id(),
                    remote_window: client_domain
                        .and_then(|domain| domain.local_to_remote_window_id(window_id)),
                    remote_tab: remote.map(|pane| pane.remote_tab_id),
                    remote_pane: remote.map(|pane| pane.remote_pane_id()),
                    client: remote.is_some(),
                });
            }
        }
    }
    MuxCensus { domains, panes }
}

/// The pane the bound window sends keyboard input to, as this phone
/// mirrors it.  Debug evidence only.
#[cfg(debug_assertions)]
#[derive(Debug, Serialize)]
pub struct ActivePane {
    /// Local pane id.
    pub pane: usize,
    /// The laptop's pane id; `None` for a pane that is not remote.
    pub remote_pane: Option<usize>,
    /// Viewport height in cells.
    pub rows: usize,
    /// Viewport width in cells.
    pub cols: usize,
    /// The cursor's viewport row.
    pub cursor_row: isize,
    /// The cursor's column.
    pub cursor_col: usize,
    /// Up to two viewports of scrollback, then the viewport's rows.  A row
    /// that wraps onto the next keeps its trailing blanks; other rows are
    /// trimmed.
    pub lines: Vec<String>,
    /// Index in `lines` of the viewport's first row.
    pub viewport_start: usize,
    /// Whether each row continues on the next one.
    pub wrapped: Vec<bool>,
}

/// GUI thread: the bound window's active pane.
#[cfg(debug_assertions)]
pub(crate) fn active_pane() -> anyhow::Result<ActivePane> {
    use wezterm_client::pane::ClientPane;
    let (_, pane) = wezterm_gui::android::bound_pane()?;
    let dims = pane.get_dimensions();
    let top = dims.physical_top;
    let cursor = pane.get_cursor_position();
    let first = dims
        .scrollback_top
        .max(top - 2 * dims.viewport_rows as isize);
    let (first, rows) = pane.get_lines(first..top + dims.viewport_rows as isize);
    let wrapped: Vec<bool> = rows.iter().map(|row| row.last_cell_was_wrapped()).collect();
    let lines = rows
        .iter()
        .zip(&wrapped)
        .map(|(row, wrapped)| {
            let text = row.as_str();
            if *wrapped {
                text.into_owned()
            } else {
                text.trim_end_matches(' ').to_string()
            }
        })
        .collect();
    Ok(ActivePane {
        pane: pane.pane_id(),
        remote_pane: pane
            .downcast_ref::<ClientPane>()
            .map(|pane| pane.remote_pane_id()),
        rows: dims.viewport_rows,
        cols: dims.cols,
        cursor_row: cursor.y - top,
        cursor_col: cursor.x,
        lines,
        viewport_start: usize::try_from(top - first).unwrap_or(0),
        wrapped,
    })
}
