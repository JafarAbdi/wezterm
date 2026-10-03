//! The one connection to the laptop mux and the prompt it may be waiting on.
//!
//! [`Connections`] owns the phase of the connection and, while attaching,
//! the single prompt the SSH session is blocked on.  A prompt is registered
//! here, with the callback that resumes the session, before the platform
//! hears of it, and it leaves in exactly one way: an answer or a
//! cancellation carrying its attempt and prompt ids, the end of its
//! attempt, or the end of the engine.  A late or repeated answer finds no
//! prompt and is refused.
//!
//! An attempt has owners: the threads that serve it, counted by its
//! [`Cancel`], and the domain it registered with the mux.  An attempt that
//! ends (cancelled, failed or disconnected) is closing until every owner is
//! gone, and only then ended; no new attempt starts before.  When its
//! threads have ended and its domain is still registered, the `retire`
//! hook asks the GUI thread to unregister it, and [`Connections::retired`]
//! reports that.  A domain the engine took down with it is stranded: it
//! cannot be reached or counted any more, and it does not hold the attempt.
//! Cancelling is refused once the attach published its panes; nothing it
//! would publish is published after an accepted cancel.
//!
//! Surfaces and Activities are not part of this state: they come and go
//! without starting, ending or answering anything.

#![forbid(unsafe_code)]

use serde::Serialize;
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use thiserror::Error;
use wezterm_ssh::{Cancel, Worker};

/// What the user is asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptKind {
    /// Trust a host whose key is not known yet?
    HostTrust {
        /// `host:port` of the server.
        remote_address: String,
        /// Fingerprint of the key it presented.
        fingerprint: String,
    },
    /// A password or passphrase; `text` is the server's or key's prompt.
    Secret {
        /// The prompt to show.
        text: String,
    },
    /// An echoed answer to `text`.
    Text {
        /// The prompt to show.
        text: String,
    },
}

/// What the user answered.
#[derive(Clone, PartialEq, Eq)]
pub enum Answer {
    /// To a [`PromptKind::HostTrust`].
    Trust(bool),
    /// To a [`PromptKind::Secret`] or [`PromptKind::Text`].
    Text(String),
}

impl std::fmt::Debug for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trust(trust) => write!(f, "Trust({trust})"),
            Self::Text(_) => f.write_str("Text(<redacted>)"),
        }
    }
}

impl Answer {
    fn fits(&self, kind: &PromptKind) -> bool {
        matches!(
            (self, kind),
            (Self::Trust(_), PromptKind::HostTrust { .. })
                | (
                    Self::Text(_),
                    PromptKind::Secret { .. } | PromptKind::Text { .. }
                )
        )
    }
}

/// Why a prompt ended without an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PromptEnded {
    /// The user cancelled it.
    #[error("the prompt was cancelled")]
    Cancelled,
    /// Its attempt is not attaching (it ended, or the engine did), or the
    /// attempt is already waiting on another prompt.
    #[error("the connection attempt is not accepting prompts")]
    NotAsking,
}

/// Resumes whoever asked.  Called exactly once, outside any lock.
pub type Responder = Box<dyn FnOnce(Result<Answer, PromptEnded>) + Send>;

/// Why an answer or cancellation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AnswerRefused {
    /// No prompt with these ids is pending: it was answered, cancelled, or
    /// its attempt ended.
    #[error("no such prompt is pending")]
    NoSuchPrompt,
    /// The answer is not of the prompt's kind; the prompt stays pending.
    #[error("the answer does not fit the prompt")]
    WrongKind,
}

/// Why [`Connections::begin`] started nothing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Refused {
    /// An attempt is attaching or attached, or the last one has not closed
    /// yet: one of its threads or its mux domain remains.
    #[error("a connection is attaching, attached or still closing")]
    Busy,
    /// The attempt's cancellation could not be created.
    #[error("the connection cannot be cancelled: {0}")]
    Uncancellable(String),
}

/// Why a cancel or a disconnect was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EndRefused {
    /// The attempt is not the one in that phase.
    #[error("that connection is not in a phase it can leave this way")]
    NotThere,
    /// The attach already published its panes; disconnect instead.
    #[error("the connection was already attached")]
    Published,
}

/// The class of an attach failure, for distinct presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The host was not reached or the SSH handshake failed.
    Unreachable,
    /// The user declined to trust an unknown host key.
    HostKeyRejected,
    /// The host presented a key that differs from the trusted one.
    HostKeyChanged,
    /// The host refused the credentials.
    Authentication,
    /// The user cancelled a credential prompt.
    AuthenticationCancelled,
    /// The laptop's mux server did not answer.
    ServerUnavailable,
    /// The laptop's wezterm speaks another mux protocol version.
    IncompatibleVersion,
    /// The GUI engine ended.
    EngineEnded,
    /// Anything else.
    Other,
}

/// Why an attached connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The transport ended on its own: the network, the laptop or an
    /// engine failure.
    Lost,
    /// The user disconnected.
    User,
}

/// Why an attempt did not attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// The class.
    pub kind: FailureKind,
    /// The error text of the layer that failed.
    pub message: String,
}

struct Pending {
    id: u64,
    kind: PromptKind,
    responder: Responder,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum End {
    Cancelled,
    Failed(Failure),
    Disconnected(Cause),
}

enum Phase {
    Idle,
    Attaching {
        attempt: u64,
        progress: String,
        prompt: Option<Pending>,
    },
    Attached {
        attempt: u64,
        windows: usize,
    },
    /// The attempt ended and some owner of it remains.
    Closing {
        attempt: u64,
        end: End,
    },
    /// The attempt ended and every owner of it is gone.
    Ended {
        attempt: u64,
        end: End,
    },
}

/// The mux domain of an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Domain {
    /// None is registered: not yet, never, or no more.
    None,
    /// Registered under this mux domain id.
    Registered(usize),
    /// The engine ended while it was registered.
    Stranded,
}

/// What holds the latest attempt open.
struct Owners {
    attempt: u64,
    cancel: Cancel,
    threads_ended: bool,
    domain: Domain,
}

impl Owners {
    fn closed(&self) -> bool {
        self.threads_ended && !matches!(self.domain, Domain::Registered(_))
    }
}

/// A pending prompt as the platform sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptView {
    /// One-use id; answers must carry it.
    pub id: u64,
    /// What is asked.
    #[serde(flatten)]
    pub kind: PromptKind,
}

/// The phase as the platform sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum PhaseView {
    /// No attempt was made.
    Idle,
    /// An attempt is in flight.
    Attaching {
        /// Its id.
        attempt: u64,
        /// The last progress line.
        progress: String,
        /// The prompt it waits on.
        prompt: Option<PromptView>,
    },
    /// The laptop's panes are mirrored.
    Attached {
        /// The attempt that attached.
        attempt: u64,
        /// Mux windows the laptop currently has; zero is an empty server.
        windows: usize,
    },
    /// The attempt was cancelled; its threads or its domain remain.
    Cancelling {
        /// Its id.
        attempt: u64,
    },
    /// The attempt was cancelled and nothing of it remains.
    Cancelled {
        /// Its id.
        attempt: u64,
    },
    /// The attempt failed; its threads or its domain remain.
    Failing {
        /// Its id.
        attempt: u64,
        /// Why.
        failure: Failure,
    },
    /// The attempt failed and nothing of it remains.
    Failed {
        /// Its id.
        attempt: u64,
        /// Why.
        failure: Failure,
    },
    /// The attached connection ended; its threads or its domain remain.
    Disconnecting {
        /// The attempt that had attached.
        attempt: u64,
        /// Why.
        cause: Cause,
    },
    /// The attached connection ended and nothing of it remains.
    Disconnected {
        /// The attempt that had attached.
        attempt: u64,
        /// Why.
        cause: Cause,
    },
}

/// Whether the latest attempt holds a mux domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainView {
    /// It holds none.
    None,
    /// Its domain is registered with the mux.
    Registered,
    /// The engine ended while its domain was registered: the domain is
    /// unreachable and its count unknown.
    Stranded,
}

/// Phase plus a revision that grows with every change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    /// Grows with every change; see [`Connections::await_change`].
    pub revision: u64,
    /// The phase.
    #[serde(flatten)]
    pub phase: PhaseView,
    /// Threads of the latest attempt that have not ended.
    pub workers: usize,
    /// The latest attempt's mux domain.
    pub domain: DomainView,
    /// A thread or the domain of the latest attempt remains: a new attempt
    /// is refused.
    pub closing: bool,
}

struct State {
    phase: Phase,
    revision: u64,
    last_attempt: u64,
    last_prompt: u64,
    owners: Option<Owners>,
}

impl State {
    /// Move a closing attempt whose owners are gone to ended, and forget
    /// owners that hold nothing any more, which also closes the wake
    /// descriptors of their cancellation.
    fn settle(&mut self) {
        if self.owners.as_ref().is_some_and(|o| !o.closed()) {
            return;
        }
        if let Phase::Closing { attempt, end } = &self.phase {
            self.phase = Phase::Ended {
                attempt: *attempt,
                end: end.clone(),
            };
        }
        let held = self
            .owners
            .as_ref()
            .is_some_and(|o| o.domain == Domain::Stranded);
        if matches!(self.phase, Phase::Ended { .. }) && !held {
            self.owners = None;
        }
    }

    /// End the current attempt as `end`; closing until its owners are gone.
    fn close(&mut self, attempt: u64, end: End) {
        self.phase = Phase::Closing { attempt, end };
        self.settle();
    }
}

/// What a change released, applied after the lock is dropped.
#[derive(Default)]
struct Effects {
    changed: bool,
    responder: Option<(Responder, Result<Answer, PromptEnded>)>,
    retire: Option<(u64, usize)>,
}

/// The connection state of the process.
pub struct Connections {
    state: Mutex<State>,
    changed: Condvar,
    /// Tells the platform that the snapshot changed.
    notify: fn(),
    /// Asks the GUI thread to unregister mux domain `.1` of attempt `.0`
    /// and then call [`Connections::retired`].
    retire: fn(u64, usize),
}

impl Connections {
    /// Idle; `notify` runs after every change and `retire` when a domain
    /// is to be unregistered, both outside the lock.
    pub const fn new(notify: fn(), retire: fn(u64, usize)) -> Self {
        Self {
            state: Mutex::new(State {
                phase: Phase::Idle,
                revision: 0,
                last_attempt: 0,
                last_prompt: 0,
                owners: None,
            }),
            changed: Condvar::new(),
            notify,
            retire,
        }
    }

    /// Apply `change` to the state.  When it reports a change, bump the
    /// revision, wake waiters and notify the platform.  Whatever responder
    /// or retirement the change released runs last, outside the lock.
    fn update<T>(&self, change: impl FnOnce(&mut State, &mut Effects) -> T) -> T {
        let mut effects = Effects::default();
        let value = {
            let mut state = self.state.lock().unwrap();
            let value = change(&mut state, &mut effects);
            if effects.changed {
                state.revision += 1;
                self.changed.notify_all();
            }
            value
        };
        if effects.changed {
            (self.notify)();
        }
        if let Some((responder, outcome)) = effects.responder {
            responder(outcome);
        }
        if let Some((attempt, domain)) = effects.retire {
            (self.retire)(attempt, domain);
        }
        value
    }

    /// Start an attempt unless another one is under way or the last one
    /// has not closed.  The returned cancellation counts the attempt's
    /// threads; when the last one ends this state learns it.  The caller
    /// is the first of them until it drops the returned worker, so an
    /// attempt that starts no thread still ends.
    pub fn begin(&'static self) -> Result<(u64, Cancel, Worker), Refused> {
        self.update(|state, effects| {
            if !matches!(state.phase, Phase::Idle | Phase::Ended { .. }) {
                return Err(Refused::Busy);
            }
            let attempt = state.last_attempt + 1;
            let cancel = match Cancel::new(move || self.released(attempt)) {
                Ok(cancel) => cancel,
                Err(err) => return Err(Refused::Uncancellable(format!("{err:#}"))),
            };
            state.last_attempt = attempt;
            state.phase = Phase::Attaching {
                attempt,
                progress: String::new(),
                prompt: None,
            };
            state.owners = Some(Owners {
                attempt,
                cancel: cancel.clone(),
                threads_ended: false,
                domain: Domain::None,
            });
            effects.changed = true;
            let starter = cancel.worker();
            Ok((attempt, cancel, starter))
        })
    }

    /// Record a progress line of `attempt`; ignored unless it is attaching.
    pub fn progress(&self, attempt: u64, line: &str) {
        self.update(|state, effects| {
            if let Phase::Attaching {
                attempt: current,
                progress,
                ..
            } = &mut state.phase
                && *current == attempt
                && progress != line
            {
                *progress = line.to_string();
                effects.changed = true;
            }
        })
    }

    /// Register a prompt of `attempt` and make it visible.  `responder`
    /// is owned here from this call on.  It is refused at once unless
    /// `attempt` is attaching and waits on nothing else.
    pub fn ask(&self, attempt: u64, kind: PromptKind, responder: Responder) {
        self.update(|state, effects| {
            let id = state.last_prompt + 1;
            match &mut state.phase {
                Phase::Attaching {
                    attempt: current,
                    prompt: prompt @ None,
                    ..
                } if *current == attempt => {
                    *prompt = Some(Pending {
                        id,
                        kind,
                        responder,
                    });
                    state.last_prompt = id;
                    effects.changed = true;
                }
                _ => effects.responder = Some((responder, Err(PromptEnded::NotAsking))),
            }
        })
    }

    fn resolve(
        &self,
        attempt: u64,
        prompt: u64,
        outcome: Result<Answer, PromptEnded>,
    ) -> Result<(), AnswerRefused> {
        self.update(|state, effects| {
            let Phase::Attaching {
                attempt: current,
                prompt: pending,
                ..
            } = &mut state.phase
            else {
                return Err(AnswerRefused::NoSuchPrompt);
            };
            let Some(asked) = pending
                .as_ref()
                .filter(|p| *current == attempt && p.id == prompt)
            else {
                return Err(AnswerRefused::NoSuchPrompt);
            };
            if matches!(&outcome, Ok(answer) if !answer.fits(&asked.kind)) {
                return Err(AnswerRefused::WrongKind);
            }
            let responder = pending.take().expect("matched above").responder;
            effects.changed = true;
            effects.responder = Some((responder, outcome));
            Ok(())
        })
    }

    /// Answer prompt `prompt` of `attempt`.  Each prompt takes one answer.
    pub fn answer(&self, attempt: u64, prompt: u64, answer: Answer) -> Result<(), AnswerRefused> {
        self.resolve(attempt, prompt, Ok(answer))
    }

    /// Cancel prompt `prompt` of `attempt`; whoever asked sees
    /// [`PromptEnded::Cancelled`].
    pub fn cancel(&self, attempt: u64, prompt: u64) -> Result<(), AnswerRefused> {
        self.resolve(attempt, prompt, Err(PromptEnded::Cancelled))
    }

    /// End `attempt`: attached with `windows` mux windows, or failed, which
    /// shuts its transport down.  A prompt still pending is refused.  False
    /// when `attempt` is not the attaching one: a completion after a cancel
    /// changes nothing.
    pub fn finish(&self, attempt: u64, outcome: Result<usize, Failure>) -> bool {
        self.update(|state, effects| {
            let (
                Phase::Attaching {
                    attempt: current,
                    prompt,
                    ..
                },
                Some(owners),
            ) = (&mut state.phase, &state.owners)
            else {
                return false;
            };
            if *current != attempt {
                return false;
            }
            effects.changed = true;
            effects.responder = prompt
                .take()
                .map(|p| (p.responder, Err(PromptEnded::NotAsking)));
            match outcome {
                Ok(_) if owners.threads_ended => {
                    state.close(attempt, End::Disconnected(Cause::Lost));
                }
                Ok(windows) => state.phase = Phase::Attached { attempt, windows },
                Err(failure) => {
                    owners.cancel.shutdown();
                    state.close(attempt, End::Failed(failure));
                }
            }
            true
        })
    }

    /// Cancel `attempt` while it attaches: its pending prompt ends as
    /// cancelled and its transport shuts down.  Refused once the attach
    /// has published its panes.
    pub fn cancel_attempt(&self, attempt: u64) -> Result<(), EndRefused> {
        self.update(|state, effects| {
            let (Phase::Attaching { prompt, .. }, Some(owners)) = (&mut state.phase, &state.owners)
            else {
                return Err(EndRefused::NotThere);
            };
            if owners.attempt != attempt {
                return Err(EndRefused::NotThere);
            }
            if !owners.cancel.cancel() {
                return Err(EndRefused::Published);
            }
            effects.changed = true;
            effects.responder = prompt
                .take()
                .map(|p| (p.responder, Err(PromptEnded::Cancelled)));
            state.close(attempt, End::Cancelled);
            Ok(())
        })
    }

    /// The user disconnects the attached `attempt`.  Returns its
    /// cancellation: the caller detaches the domain, then shuts the
    /// transport down; the attempt is disconnected once its owners are gone.
    pub fn disconnect(&self, attempt: u64) -> Result<Cancel, EndRefused> {
        self.update(|state, effects| match (&state.phase, &state.owners) {
            (Phase::Attached { .. }, Some(owners)) if owners.attempt == attempt => {
                let cancel = owners.cancel.clone();
                effects.changed = true;
                state.close(attempt, End::Disconnected(Cause::User));
                Ok(cancel)
            }
            _ => Err(EndRefused::NotThere),
        })
    }

    /// The GUI thread registered mux domain `domain` for `attempt`.
    pub fn registered(&self, attempt: u64, domain: usize) {
        self.update(|state, effects| {
            let Some(owners) = state
                .owners
                .as_mut()
                .filter(|o| o.attempt == attempt && o.domain == Domain::None)
            else {
                return;
            };
            owners.domain = Domain::Registered(domain);
            effects.changed = true;
            if owners.threads_ended {
                effects.retire = Some((attempt, domain));
            }
        })
    }

    /// The GUI thread unregistered the domain of `attempt`.
    pub fn retired(&self, attempt: u64) {
        self.update(|state, effects| {
            let Some(owners) = state
                .owners
                .as_mut()
                .filter(|o| o.attempt == attempt && matches!(o.domain, Domain::Registered(_)))
            else {
                return;
            };
            owners.domain = Domain::None;
            effects.changed = true;
            state.settle();
        })
    }

    /// Every thread of `attempt` has ended.
    fn released(&self, attempt: u64) {
        self.update(|state, effects| {
            let Some(owners) = state.owners.as_mut().filter(|o| o.attempt == attempt) else {
                return;
            };
            owners.threads_ended = true;
            effects.changed = true;
            if let Domain::Registered(domain) = owners.domain {
                effects.retire = Some((attempt, domain));
            }
            match state.phase {
                Phase::Attached { .. } => state.close(attempt, End::Disconnected(Cause::Lost)),
                _ => state.settle(),
            }
        })
    }

    /// The laptop now has `windows` mux windows; ignored unless `attempt`
    /// is attached.
    pub fn windows(&self, attempt: u64, windows: usize) {
        self.update(|state, effects| {
            if let Phase::Attached {
                attempt: current,
                windows: known,
            } = &mut state.phase
                && *current == attempt
                && *known != windows
            {
                *known = windows;
                effects.changed = true;
            }
        })
    }

    /// Shut down the transport of the latest attempt whatever its phase,
    /// as a failing network would; false when its threads already ended.
    #[cfg(any(test, debug_assertions))]
    pub fn interrupt_transport(&self) -> bool {
        let state = self.state.lock().unwrap();
        match &state.owners {
            Some(owners) if !owners.threads_ended => {
                owners.cancel.shutdown();
                true
            }
            _ => false,
        }
    }

    /// The GUI engine ended: an attaching attempt fails and its prompt is
    /// refused; an attached connection is lost.  Either way the transport
    /// shuts down, and a registered domain is stranded, since no GUI task
    /// will detach or unregister it any more.
    pub fn engine_ended(&self, message: &str) {
        self.update(|state, effects| {
            let Some(owners) = state.owners.as_mut() else {
                return;
            };
            if !owners.threads_ended && !owners.cancel.cancel() {
                owners.cancel.shutdown();
            }
            if matches!(owners.domain, Domain::Registered(_)) {
                owners.domain = Domain::Stranded;
                effects.changed = true;
            }
            let attempt = owners.attempt;
            match &mut state.phase {
                Phase::Attaching { prompt, .. } => {
                    effects.responder = prompt
                        .take()
                        .map(|p| (p.responder, Err(PromptEnded::NotAsking)));
                    let failure = Failure {
                        kind: FailureKind::EngineEnded,
                        message: message.to_string(),
                    };
                    state.close(attempt, End::Failed(failure));
                    effects.changed = true;
                }
                Phase::Attached { .. } => {
                    state.close(attempt, End::Disconnected(Cause::Lost));
                    effects.changed = true;
                }
                _ => state.settle(),
            }
        })
    }

    /// The current phase.
    pub fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap();
        let phase = match &state.phase {
            Phase::Idle => PhaseView::Idle,
            Phase::Attaching {
                attempt,
                progress,
                prompt,
            } => PhaseView::Attaching {
                attempt: *attempt,
                progress: progress.clone(),
                prompt: prompt.as_ref().map(|p| PromptView {
                    id: p.id,
                    kind: p.kind.clone(),
                }),
            },
            Phase::Attached { attempt, windows } => PhaseView::Attached {
                attempt: *attempt,
                windows: *windows,
            },
            Phase::Closing { attempt, end } => {
                let attempt = *attempt;
                match end.clone() {
                    End::Cancelled => PhaseView::Cancelling { attempt },
                    End::Failed(failure) => PhaseView::Failing { attempt, failure },
                    End::Disconnected(cause) => PhaseView::Disconnecting { attempt, cause },
                }
            }
            Phase::Ended { attempt, end } => {
                let attempt = *attempt;
                match end.clone() {
                    End::Cancelled => PhaseView::Cancelled { attempt },
                    End::Failed(failure) => PhaseView::Failed { attempt, failure },
                    End::Disconnected(cause) => PhaseView::Disconnected { attempt, cause },
                }
            }
        };
        let owners = state.owners.as_ref();
        Snapshot {
            revision: state.revision,
            phase,
            workers: owners.map_or(0, |o| o.cancel.workers()),
            domain: match owners.map(|o| o.domain) {
                None | Some(Domain::None) => DomainView::None,
                Some(Domain::Registered(_)) => DomainView::Registered,
                Some(Domain::Stranded) => DomainView::Stranded,
            },
            closing: owners.is_some_and(|o| !o.closed()),
        }
    }

    /// Block until the revision differs from `since` or `timeout` elapsed;
    /// returns the revision then current.
    pub fn await_change(&self, since: u64, timeout: Duration) -> u64 {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| state.revision == since)
            .unwrap();
        state.revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, channel};

    thread_local! {
        /// Retirements asked of the GUI thread by this test thread.
        static RETIRES: RefCell<Vec<(u64, usize)>> = const { RefCell::new(Vec::new()) };
    }

    fn record_retire(attempt: u64, domain: usize) {
        RETIRES.with_borrow_mut(|r| r.push((attempt, domain)));
    }

    fn retires() -> Vec<(u64, usize)> {
        RETIRES.with_borrow_mut(std::mem::take)
    }

    fn connections() -> &'static Connections {
        Box::leak(Box::new(Connections::new(|| {}, record_retire)))
    }

    fn responder() -> (Responder, Receiver<Result<Answer, PromptEnded>>) {
        let (tx, rx) = channel();
        (
            Box::new(move |outcome| tx.send(outcome).expect("the asker waits")),
            rx,
        )
    }

    fn host_trust() -> PromptKind {
        PromptKind::HostTrust {
            remote_address: "100.64.0.1:22".into(),
            fingerprint: "SHA256:abc".into(),
        }
    }

    fn secret() -> PromptKind {
        PromptKind::Secret {
            text: "Password: ".into(),
        }
    }

    fn pending_prompt(connections: &Connections) -> Option<PromptView> {
        match connections.snapshot().phase {
            PhaseView::Attaching { prompt, .. } => prompt,
            _ => None,
        }
    }

    fn failure(kind: FailureKind) -> Failure {
        Failure {
            kind,
            message: "why".into(),
        }
    }

    /// Begin an attempt; its starter is the one thread serving it.
    fn begin(c: &'static Connections) -> (u64, Cancel, Worker) {
        c.begin().unwrap()
    }

    fn phase(c: &Connections) -> PhaseView {
        c.snapshot().phase
    }

    /// Workers, domain and closing, as the platform reads them.
    fn owners(c: &Connections) -> (usize, DomainView, bool) {
        let s = c.snapshot();
        (s.workers, s.domain, s.closing)
    }

    #[test]
    fn one_connect_operation_at_a_time_and_none_while_threads_remain() {
        let c = connections();
        let (first, cancel, worker) = begin(c);
        assert_eq!(c.begin().err(), Some(Refused::Busy));
        assert!(c.finish(first, Err(failure(FailureKind::Unreachable))));
        assert!(cancel.is_shut(), "a failed attempt's transport shuts down");
        assert_eq!(
            phase(c),
            PhaseView::Failing {
                attempt: first,
                failure: failure(FailureKind::Unreachable)
            }
        );
        assert_eq!(owners(c), (1, DomainView::None, true));
        assert_eq!(c.begin().err(), Some(Refused::Busy));
        drop(worker);
        assert_eq!(
            phase(c),
            PhaseView::Failed {
                attempt: first,
                failure: failure(FailureKind::Unreachable)
            }
        );
        assert_eq!(owners(c), (0, DomainView::None, false));
        let (second, _, worker) = begin(c);
        assert_eq!(second, first + 1);
        assert!(c.finish(second, Ok(2)));
        assert_eq!(c.begin().err(), Some(Refused::Busy));
        drop(worker);
        assert_eq!(
            phase(c),
            PhaseView::Disconnected {
                attempt: second,
                cause: Cause::Lost
            }
        );
        let (third, _, starter) = c.begin().unwrap();
        assert_eq!(third, second + 1);
        assert!(c.finish(third, Err(failure(FailureKind::EngineEnded))));
        drop(starter);
        assert!(
            !c.snapshot().closing,
            "an attempt that started no thread ends"
        );
        assert_eq!(retires(), vec![], "no domain was registered");
    }

    #[test]
    fn a_prompt_is_visible_once_registered_and_takes_one_answer() {
        let c = connections();
        let (attempt, _, _worker) = begin(c);
        let (respond, answered) = responder();
        c.ask(attempt, host_trust(), respond);
        let prompt = pending_prompt(c).expect("registered");
        assert_eq!(prompt.kind, host_trust());
        assert!(answered.try_recv().is_err());

        assert_eq!(c.answer(attempt, prompt.id, Answer::Trust(true)), Ok(()));
        assert_eq!(answered.recv().unwrap(), Ok(Answer::Trust(true)));
        assert_eq!(pending_prompt(c), None);
        assert_eq!(
            c.answer(attempt, prompt.id, Answer::Trust(false)),
            Err(AnswerRefused::NoSuchPrompt)
        );
        assert_eq!(
            c.cancel(attempt, prompt.id),
            Err(AnswerRefused::NoSuchPrompt)
        );
        assert!(answered.try_recv().is_err());
    }

    #[test]
    fn answers_must_carry_the_attempt_the_prompt_and_the_kind() {
        let c = connections();
        let (attempt, _, _worker) = begin(c);
        let (respond, answered) = responder();
        c.ask(attempt, secret(), respond);
        let prompt = pending_prompt(c).unwrap().id;

        let text = || Answer::Text("hunter2".into());
        assert_eq!(
            c.answer(attempt + 1, prompt, text()),
            Err(AnswerRefused::NoSuchPrompt)
        );
        assert_eq!(
            c.answer(attempt, prompt + 1, text()),
            Err(AnswerRefused::NoSuchPrompt)
        );
        assert_eq!(
            c.answer(attempt, prompt, Answer::Trust(true)),
            Err(AnswerRefused::WrongKind)
        );
        assert!(answered.try_recv().is_err());
        assert_eq!(pending_prompt(c).map(|p| p.id), Some(prompt));

        assert_eq!(c.answer(attempt, prompt, text()), Ok(()));
        assert_eq!(answered.recv().unwrap(), Ok(text()));
    }

    #[test]
    fn cancelling_a_prompt_resumes_the_asker_with_an_error() {
        let c = connections();
        let (attempt, _, _worker) = begin(c);
        let (respond, answered) = responder();
        c.ask(attempt, secret(), respond);
        let prompt = pending_prompt(c).unwrap().id;
        assert_eq!(c.cancel(attempt, prompt), Ok(()));
        assert_eq!(answered.recv().unwrap(), Err(PromptEnded::Cancelled));
        assert_eq!(
            c.answer(attempt, prompt, Answer::Text("late".into())),
            Err(AnswerRefused::NoSuchPrompt)
        );
    }

    #[test]
    fn cancelling_an_attempt_ends_its_prompt_shuts_its_transport_and_waits_for_its_threads() {
        let c = connections();
        let (attempt, cancel, worker) = begin(c);
        let (respond, answered) = responder();
        c.ask(attempt, host_trust(), respond);
        let prompt = pending_prompt(c).unwrap().id;

        assert_eq!(c.cancel_attempt(attempt + 1), Err(EndRefused::NotThere));
        assert_eq!(c.cancel_attempt(attempt), Ok(()));
        assert_eq!(answered.recv().unwrap(), Err(PromptEnded::Cancelled));
        assert!(cancel.is_shut());
        assert!(!cancel.commit(), "the attach can no longer publish");
        assert_eq!(phase(c), PhaseView::Cancelling { attempt });
        assert_eq!(c.begin().err(), Some(Refused::Busy));

        assert_eq!(
            c.answer(attempt, prompt, Answer::Trust(true)),
            Err(AnswerRefused::NoSuchPrompt)
        );
        let (late, late_answered) = responder();
        c.ask(attempt, secret(), late);
        assert_eq!(late_answered.recv().unwrap(), Err(PromptEnded::NotAsking));
        assert!(!c.finish(attempt, Err(failure(FailureKind::Other))));
        assert!(!c.finish(attempt, Ok(1)));
        assert_eq!(c.cancel_attempt(attempt), Err(EndRefused::NotThere));
        assert_eq!(phase(c), PhaseView::Cancelling { attempt });

        drop(worker);
        assert_eq!(phase(c), PhaseView::Cancelled { attempt });
        assert_eq!(owners(c), (0, DomainView::None, false));
        assert_eq!(c.begin().unwrap().0, attempt + 1);
    }

    #[test]
    fn a_cancel_after_the_threads_ended_is_cancelled_at_once() {
        let c = connections();
        let (attempt, _, worker) = begin(c);
        drop(worker);
        assert_eq!(c.cancel_attempt(attempt), Ok(()));
        assert_eq!(phase(c), PhaseView::Cancelled { attempt });
        assert!(!c.finish(attempt, Err(failure(FailureKind::Other))));
    }

    #[test]
    fn an_attach_that_committed_cannot_be_cancelled() {
        let c = connections();
        let (attempt, cancel, _worker) = begin(c);
        assert!(cancel.commit());
        assert_eq!(c.cancel_attempt(attempt), Err(EndRefused::Published));
        assert!(!cancel.is_shut());
        assert!(c.finish(attempt, Ok(1)));
        assert_eq!(
            phase(c),
            PhaseView::Attached {
                attempt,
                windows: 1
            }
        );
    }

    #[test]
    fn an_attach_that_fails_after_committing_shuts_its_transport() {
        let c = connections();
        let (attempt, cancel, worker) = begin(c);
        c.registered(attempt, 7);
        assert!(cancel.commit());
        assert!(c.finish(attempt, Err(failure(FailureKind::Other))));
        assert!(cancel.is_shut(), "its client thread reads end of file");
        drop(worker);
        assert_eq!(retires(), vec![(attempt, 7)]);
        assert_eq!(owners(c), (0, DomainView::Registered, true));
        c.retired(attempt);
        assert_eq!(
            phase(c),
            PhaseView::Failed {
                attempt,
                failure: failure(FailureKind::Other)
            }
        );
    }

    #[test]
    fn a_disconnect_returns_the_transport_and_ends_when_its_owners_do() {
        let c = connections();
        let (attempt, cancel, worker) = begin(c);
        c.registered(attempt, 3);
        assert_eq!(c.disconnect(attempt).err(), Some(EndRefused::NotThere));
        assert!(cancel.commit());
        assert!(c.finish(attempt, Ok(0)));
        assert_eq!(c.disconnect(attempt + 1).err(), Some(EndRefused::NotThere));
        let shut = c.disconnect(attempt).unwrap();
        let disconnecting = PhaseView::Disconnecting {
            attempt,
            cause: Cause::User,
        };
        assert_eq!(phase(c), disconnecting);
        assert_eq!(c.disconnect(attempt).err(), Some(EndRefused::NotThere));
        shut.shutdown();
        assert!(cancel.is_shut());
        assert_eq!(c.begin().err(), Some(Refused::Busy));
        drop(worker);
        assert_eq!(retires(), vec![(attempt, 3)]);
        assert_eq!(phase(c), disconnecting, "the domain is still registered");
        assert_eq!(c.begin().err(), Some(Refused::Busy));
        c.retired(attempt);
        assert_eq!(
            phase(c),
            PhaseView::Disconnected {
                attempt,
                cause: Cause::User
            }
        );
        assert_eq!(owners(c), (0, DomainView::None, false));
    }

    #[test]
    fn the_end_of_the_threads_of_an_attached_connection_is_a_lost_connection() {
        let c = connections();
        let (attempt, cancel, worker) = begin(c);
        c.registered(attempt, 5);
        assert!(cancel.commit());
        assert!(c.finish(attempt, Ok(1)));
        assert!(c.interrupt_transport());
        assert!(cancel.is_shut());
        drop(worker);
        assert_eq!(
            phase(c),
            PhaseView::Disconnecting {
                attempt,
                cause: Cause::Lost
            }
        );
        assert_eq!(retires(), vec![(attempt, 5)]);
        c.retired(attempt);
        assert_eq!(
            phase(c),
            PhaseView::Disconnected {
                attempt,
                cause: Cause::Lost
            }
        );
        assert!(!c.interrupt_transport());
        c.windows(attempt, 3);
        assert_eq!(owners(c), (0, DomainView::None, false));
    }

    #[test]
    fn threads_that_end_before_the_attach_is_reported_make_it_lost() {
        let c = connections();
        let (attempt, cancel, worker) = begin(c);
        c.registered(attempt, 2);
        assert!(cancel.commit());
        drop(worker);
        assert_eq!(
            retires(),
            vec![(attempt, 2)],
            "asked once, when the threads ended"
        );
        assert!(c.finish(attempt, Ok(1)));
        assert_eq!(
            phase(c),
            PhaseView::Disconnecting {
                attempt,
                cause: Cause::Lost
            }
        );
        c.retired(attempt);
        assert_eq!(
            phase(c),
            PhaseView::Disconnected {
                attempt,
                cause: Cause::Lost
            }
        );
        assert_eq!(retires(), vec![]);
    }

    #[test]
    fn a_cancel_after_the_domain_registered_waits_for_its_unregistration() {
        let c = connections();
        let (attempt, cancel, worker) = begin(c);
        c.registered(attempt, 11);
        assert_eq!(owners(c), (1, DomainView::Registered, true));
        assert_eq!(c.cancel_attempt(attempt), Ok(()));
        assert!(!cancel.commit(), "the attach cannot publish its panes");
        assert_eq!(retires(), vec![], "the threads still run");
        drop(worker);
        assert_eq!(retires(), vec![(attempt, 11)]);
        assert_eq!(phase(c), PhaseView::Cancelling { attempt });
        assert_eq!(owners(c), (0, DomainView::Registered, true));
        assert_eq!(c.begin().err(), Some(Refused::Busy));
        c.retired(attempt);
        assert_eq!(phase(c), PhaseView::Cancelled { attempt });
        assert_eq!(owners(c), (0, DomainView::None, false));
        c.retired(attempt);
        assert_eq!(retires(), vec![], "a repeated retirement asks nothing");
    }

    #[test]
    fn a_domain_registered_after_the_threads_ended_is_retired_at_once() {
        let c = connections();
        let (attempt, _, worker) = begin(c);
        assert_eq!(c.cancel_attempt(attempt), Ok(()));
        drop(worker);
        assert_eq!(phase(c), PhaseView::Cancelled { attempt });
        c.registered(attempt, 4);
        assert_eq!(retires(), vec![], "an ended attempt holds no domain");

        let (second, _, worker) = begin(c);
        drop(worker);
        c.registered(second, 6);
        assert_eq!(retires(), vec![(second, 6)]);
        assert_eq!(c.snapshot().domain, DomainView::Registered);
    }

    #[test]
    fn a_late_retirement_of_an_old_attempt_leaves_the_current_domain() {
        let c = connections();
        let (first, _, worker) = begin(c);
        c.registered(first, 1);
        assert_eq!(c.cancel_attempt(first), Ok(()));
        drop(worker);
        c.retired(first);
        let (second, _, _worker) = begin(c);
        c.registered(second, 2);
        c.registered(second, 9);
        c.retired(first);
        assert_eq!(owners(c), (1, DomainView::Registered, true));
        assert_eq!(c.cancel_attempt(second), Ok(()));
        drop(_worker);
        assert_eq!(
            retires(),
            vec![(first, 1), (second, 2)],
            "each attempt's own domain id, once"
        );
    }

    #[test]
    fn the_end_of_an_attempt_refuses_its_pending_prompt() {
        let c = connections();
        let (attempt, _, _worker) = begin(c);
        let (respond, answered) = responder();
        c.ask(attempt, host_trust(), respond);
        let prompt = pending_prompt(c).unwrap().id;
        assert!(c.finish(attempt, Err(failure(FailureKind::Unreachable))));
        assert_eq!(answered.recv().unwrap(), Err(PromptEnded::NotAsking));
        assert_eq!(
            c.answer(attempt, prompt, Answer::Trust(true)),
            Err(AnswerRefused::NoSuchPrompt)
        );
        assert_eq!(
            phase(c),
            PhaseView::Failing {
                attempt,
                failure: failure(FailureKind::Unreachable)
            }
        );
    }

    #[test]
    fn prompts_of_an_ended_attempt_or_behind_another_prompt_are_refused_unseen() {
        let c = connections();
        let (respond, answered) = responder();
        c.ask(1, secret(), respond);
        assert_eq!(answered.recv().unwrap(), Err(PromptEnded::NotAsking));

        let (attempt, _, _worker) = begin(c);
        let (first, first_answered) = responder();
        c.ask(attempt, host_trust(), first);
        let (second, second_answered) = responder();
        c.ask(attempt, secret(), second);
        assert_eq!(second_answered.recv().unwrap(), Err(PromptEnded::NotAsking));
        assert_eq!(pending_prompt(c).unwrap().kind, host_trust());
        assert!(first_answered.try_recv().is_err());

        assert!(c.finish(attempt, Ok(0)));
        let (late, late_answered) = responder();
        c.ask(attempt, secret(), late);
        assert_eq!(late_answered.recv().unwrap(), Err(PromptEnded::NotAsking));
    }

    #[test]
    fn a_stale_attempt_cannot_finish_or_count_windows_of_the_current_one() {
        let c = connections();
        let (first, _, worker) = begin(c);
        assert!(c.finish(first, Err(failure(FailureKind::Other))));
        drop(worker);
        let (second, _, _worker) = begin(c);
        assert!(!c.finish(first, Ok(3)));
        assert!(c.finish(second, Ok(0)));
        assert!(!c.finish(second, Err(failure(FailureKind::Other))));
        c.windows(first, 9);
        assert_eq!(
            phase(c),
            PhaseView::Attached {
                attempt: second,
                windows: 0
            }
        );
        c.windows(second, 2);
        assert_eq!(
            phase(c),
            PhaseView::Attached {
                attempt: second,
                windows: 2
            }
        );
    }

    #[test]
    fn the_end_of_the_engine_fails_the_attempt_and_waits_for_its_threads() {
        let c = connections();
        let (attempt, cancel, worker) = begin(c);
        let (respond, answered) = responder();
        c.ask(attempt, secret(), respond);
        c.engine_ended("GUI thread panicked");
        assert_eq!(answered.recv().unwrap(), Err(PromptEnded::NotAsking));
        assert!(cancel.is_shut());
        assert!(!cancel.commit());
        let ended = Failure {
            kind: FailureKind::EngineEnded,
            message: "GUI thread panicked".into(),
        };
        assert_eq!(
            phase(c),
            PhaseView::Failing {
                attempt,
                failure: ended.clone()
            }
        );
        drop(worker);
        assert_eq!(
            phase(c),
            PhaseView::Failed {
                attempt,
                failure: ended
            }
        );

        let attached = connections();
        let (attempt, cancel, worker) = begin(attached);
        attached.registered(attempt, 8);
        assert!(cancel.commit());
        assert!(attached.finish(attempt, Ok(1)));
        attached.engine_ended("stopped");
        assert!(cancel.is_shut());
        assert_eq!(owners(attached), (1, DomainView::Stranded, true));
        drop(worker);
        assert_eq!(retires(), vec![], "no GUI task can unregister it");
        assert_eq!(
            phase(attached),
            PhaseView::Disconnected {
                attempt,
                cause: Cause::Lost
            }
        );
        assert_eq!(
            owners(attached),
            (0, DomainView::Stranded, false),
            "the stranded domain is reported, not counted as gone"
        );
        attached.retired(attempt);
        assert_eq!(attached.snapshot().domain, DomainView::Stranded);
        assert_eq!(attached.begin().map(|b| b.0).ok(), Some(attempt + 1));

        let idle = connections();
        idle.engine_ended("stopped");
        assert_eq!(phase(idle), PhaseView::Idle);
        assert_eq!(idle.snapshot().revision, 0);
    }

    #[test]
    fn the_end_of_the_engine_strands_a_domain_whose_retirement_never_ran() {
        let c = connections();
        let (attempt, _, worker) = begin(c);
        c.registered(attempt, 12);
        assert_eq!(c.cancel_attempt(attempt), Ok(()));
        drop(worker);
        assert_eq!(retires(), vec![(attempt, 12)]);
        c.engine_ended("GUI thread panicked");
        assert_eq!(phase(c), PhaseView::Cancelled { attempt });
        assert_eq!(owners(c), (0, DomainView::Stranded, false));
    }

    #[test]
    fn repeated_cancels_and_disconnects_leave_no_owner_behind() {
        let c = connections();
        for cycle in 0..3_usize {
            let (attempt, cancel, worker) = begin(c);
            c.registered(attempt, 100 + cycle);
            let (respond, answered) = responder();
            c.ask(attempt, secret(), respond);
            assert_eq!(c.cancel_attempt(attempt), Ok(()));
            assert_eq!(answered.recv().unwrap(), Err(PromptEnded::Cancelled));
            drop(worker);
            assert_eq!(retires(), vec![(attempt, 100 + cycle)]);
            c.retired(attempt);
            assert_eq!(phase(c), PhaseView::Cancelled { attempt });
            assert_eq!(owners(c), (0, DomainView::None, false));
            assert_eq!(cancel.workers(), 0);

            let (attempt, cancel, worker) = begin(c);
            c.registered(attempt, 200 + cycle);
            assert!(cancel.commit());
            assert!(c.finish(attempt, Ok(1)));
            c.disconnect(attempt).unwrap().shutdown();
            drop(worker);
            assert_eq!(retires(), vec![(attempt, 200 + cycle)]);
            c.retired(attempt);
            assert_eq!(
                phase(c),
                PhaseView::Disconnected {
                    attempt,
                    cause: Cause::User
                }
            );
            assert_eq!(owners(c), (0, DomainView::None, false));
        }
        assert_eq!(
            c.snapshot().phase,
            PhaseView::Disconnected {
                attempt: 6,
                cause: Cause::User
            }
        );
    }

    #[test]
    fn every_change_bumps_the_revision_notifies_and_wakes_waiters() {
        static NOTIFIED: AtomicUsize = AtomicUsize::new(0);
        let c: &'static Connections = Box::leak(Box::new(Connections::new(
            || {
                NOTIFIED.fetch_add(1, Ordering::SeqCst);
            },
            record_retire,
        )));
        assert_eq!(c.await_change(0, Duration::ZERO), 0);
        let waiter = std::thread::spawn(move || c.await_change(0, Duration::from_secs(60)));
        let (attempt, _, starter) = c.begin().unwrap();
        assert_eq!(waiter.join().unwrap(), 1);
        c.progress(attempt, "Connecting");
        c.progress(attempt, "Connecting");
        c.progress(attempt + 1, "stale");
        assert_eq!(c.snapshot().revision, 2);
        assert_eq!(NOTIFIED.load(Ordering::SeqCst), 2);
        drop(starter);
        assert_eq!(
            c.snapshot().revision,
            3,
            "the end of its threads is a change"
        );
    }

    #[test]
    fn snapshots_serialize_flat_and_answers_never_print_their_text() {
        let c = connections();
        let (attempt, _, worker) = begin(c);
        let (respond, _answered) = responder();
        c.ask(attempt, host_trust(), respond);
        assert_eq!(
            serde_json::to_value(c.snapshot()).unwrap(),
            serde_json::json!({
                "revision": 2,
                "phase": "attaching",
                "attempt": 1,
                "progress": "",
                "prompt": {
                    "id": 1,
                    "kind": "host_trust",
                    "remote_address": "100.64.0.1:22",
                    "fingerprint": "SHA256:abc",
                },
                "workers": 1,
                "domain": "none",
                "closing": true,
            })
        );
        c.registered(attempt, 1);
        assert_eq!(c.cancel_attempt(attempt), Ok(()));
        assert_eq!(
            serde_json::to_value(c.snapshot()).unwrap(),
            serde_json::json!({
                "revision": 4,
                "phase": "cancelling",
                "attempt": 1,
                "workers": 1,
                "domain": "registered",
                "closing": true,
            })
        );
        drop(worker);
        c.retired(attempt);
        assert_eq!(
            serde_json::to_value(c.snapshot()).unwrap(),
            serde_json::json!({
                "revision": 6,
                "phase": "cancelled",
                "attempt": 1,
                "workers": 0,
                "domain": "none",
                "closing": false,
            })
        );
        let (second, cancel, worker) = begin(c);
        assert!(cancel.commit());
        assert!(c.finish(second, Err(failure(FailureKind::Authentication))));
        assert_eq!(
            serde_json::to_value(c.snapshot()).unwrap(),
            serde_json::json!({
                "revision": 8,
                "phase": "failing",
                "attempt": 2,
                "failure": {"kind": "authentication", "message": "why"},
                "workers": 1,
                "domain": "none",
                "closing": true,
            })
        );
        drop(worker);
        let (third, cancel, worker) = begin(c);
        assert!(cancel.commit());
        assert!(c.finish(third, Ok(1)));
        c.disconnect(third).unwrap();
        assert_eq!(
            serde_json::to_value(c.snapshot()).unwrap(),
            serde_json::json!({
                "revision": 12,
                "phase": "disconnecting",
                "attempt": 3,
                "cause": "user",
                "workers": 1,
                "domain": "none",
                "closing": true,
            })
        );
        drop(worker);
        assert_eq!(
            serde_json::to_value(c.snapshot()).unwrap(),
            serde_json::json!({
                "revision": 13,
                "phase": "disconnected",
                "attempt": 3,
                "cause": "user",
                "workers": 0,
                "domain": "none",
                "closing": false,
            })
        );
        assert_eq!(
            format!("{:?}", Answer::Text("hunter2".into())),
            "Text(<redacted>)"
        );
    }
}
