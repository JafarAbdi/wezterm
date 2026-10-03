//! Attaching to the laptop mux: the one SSHMUX `ClientDomain`.
//!
//! [`connect`] validates the profile, starts the only connect operation
//! and returns.  The attach itself is the desktop one
//! (`ClientDomain::attach_with_ui`): native SSH, native `known_hosts`
//! verification, the codec version check, then the pane list.  Its
//! `ConnectionUI` requests are consumed here and become state in
//! [`Connections`]; Kotlin shows that state and answers prompts by id.
//!
//! No other domain is ever added to the mux, nothing is spawned when the
//! laptop has no panes, and the proxy command never starts a server.

#![forbid(unsafe_code)]

use crate::connection::{
    Answer, AnswerRefused, Busy, Connections, Failure, FailureKind, PromptKind, Responder, Snapshot,
};
use crate::engine::EngineState;
use crate::profile::{Profile, ProfileError, ProfileFields};
use crate::sshstore::{ImportError, SshStore};
use config::SshDomain;
use mux::connui::{ConnectionUI, UIRequest};
use mux::domain::{Domain, DomainState};
use mux::ssh::{SshConnectError, SshConnectStage};
use mux::{Mux, MuxNotification};
use serde::Serialize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use termwiz::surface::Change;
use thiserror::Error;
use wezterm_client::client::{IncompatibleVersionError, VersionCheckFailed};
use wezterm_client::domain::{ClientDomain, ClientDomainConfig};
use window::os::android::platform_requests;

static CONNECTION: Connections = Connections::new(|| platform_requests().connection_changed());
static STORE: OnceLock<SshStore> = OnceLock::new();

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
}

/// Why [`connect`] started nothing.
#[derive(Debug, Error)]
pub enum ConnectRefused {
    /// A profile field is invalid.
    #[error(transparent)]
    Profile(#[from] ProfileError),
    /// A connection is attaching or attached.
    #[error(transparent)]
    Busy(#[from] Busy),
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
    let attempt = CONNECTION.begin()?;
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
        &format!("laptop-{attempt}"),
        &store.known_hosts(),
        store.identity().as_deref(),
    );
    let (ui, requests) = ConnectionUI::with_consumer();
    let consumer = std::thread::Builder::new()
        .name("wezterm-connect-ui".into())
        .spawn(move || consume(attempt, requests));
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
            log::error!("attach attempt {attempt} failed: {:?}", failure.kind);
        }
        CONNECTION.finish(attempt, outcome);
    })
    .detach();
    Ok(attempt)
}

/// GUI thread: register the domain, attach, and keep the connection state
/// in step with the mux.  Returns the number of mux windows.
async fn attach(attempt: u64, ssh: SshDomain, ui: ConnectionUI) -> anyhow::Result<usize> {
    let mux = Mux::get();
    let domain: Arc<dyn Domain> = Arc::new(ClientDomain::new(ClientDomainConfig::Ssh(ssh)));
    mux.add_domain(&domain);
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
                } else if live.swap(false, Ordering::SeqCst) {
                    CONNECTION.detached(attempt);
                }
            })
            .detach();
        }
        live.load(Ordering::SeqCst)
    });
    Ok(mux.iter_windows().len())
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
/// drops its last clone.  A prompt's promise moves into [`Connections`]
/// before the platform can see the prompt.
fn consume(attempt: u64, requests: impl Iterator<Item = UIRequest>) {
    for request in requests {
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
                mut respond,
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
                mut respond,
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
            UIRequest::Sleep { mut respond, .. } => {
                // Only reconnect loops sleep, and SSHMUX has none.
                respond.err(anyhow::anyhow!("the Android connection UI does not wait"));
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
