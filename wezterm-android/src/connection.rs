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
//! Surfaces and Activities are not part of this state: they come and go
//! without starting, ending or answering anything.

#![forbid(unsafe_code)]

use serde::Serialize;
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use thiserror::Error;

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

/// A connect operation is active or the connection is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a connection is attaching or attached")]
pub struct Busy;

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
    Failed {
        attempt: u64,
        failure: Failure,
    },
    Disconnected {
        attempt: u64,
    },
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
    /// The last attempt failed.
    Failed {
        /// Its id.
        attempt: u64,
        /// Why.
        failure: Failure,
    },
    /// The attached connection ended.
    Disconnected {
        /// The attempt that had attached.
        attempt: u64,
    },
}

/// Phase plus a revision that grows with every change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    /// Grows with every change; see [`Connections::await_change`].
    pub revision: u64,
    /// The phase.
    #[serde(flatten)]
    pub phase: PhaseView,
}

struct State {
    phase: Phase,
    revision: u64,
    last_attempt: u64,
    last_prompt: u64,
}

/// The connection state of the process.
pub struct Connections {
    state: Mutex<State>,
    changed: Condvar,
    /// Tells the platform that the snapshot changed.
    notify: fn(),
}

impl Connections {
    /// Idle; `notify` runs after every change, outside the lock.
    pub const fn new(notify: fn()) -> Self {
        Self {
            state: Mutex::new(State {
                phase: Phase::Idle,
                revision: 0,
                last_attempt: 0,
                last_prompt: 0,
            }),
            changed: Condvar::new(),
            notify,
        }
    }

    /// Apply `change` to the state.  When it reports a change, bump the
    /// revision, wake waiters and notify the platform.  Whatever responder
    /// the change released is called last, outside the lock.
    fn update<T>(
        &self,
        change: impl FnOnce(&mut State) -> (bool, Option<(Responder, Result<Answer, PromptEnded>)>, T),
    ) -> T {
        let (changed, release, value) = {
            let mut state = self.state.lock().unwrap();
            let (changed, release, value) = change(&mut state);
            if changed {
                state.revision += 1;
                self.changed.notify_all();
            }
            (changed, release, value)
        };
        if changed {
            (self.notify)();
        }
        if let Some((responder, outcome)) = release {
            responder(outcome);
        }
        value
    }

    /// Start an attempt unless one is attaching or attached.
    pub fn begin(&self) -> Result<u64, Busy> {
        self.update(|state| match state.phase {
            Phase::Attaching { .. } | Phase::Attached { .. } => (false, None, Err(Busy)),
            Phase::Idle | Phase::Failed { .. } | Phase::Disconnected { .. } => {
                state.last_attempt += 1;
                state.phase = Phase::Attaching {
                    attempt: state.last_attempt,
                    progress: String::new(),
                    prompt: None,
                };
                (true, None, Ok(state.last_attempt))
            }
        })
    }

    /// Record a progress line of `attempt`; ignored unless it is attaching.
    pub fn progress(&self, attempt: u64, line: &str) {
        self.update(|state| match &mut state.phase {
            Phase::Attaching {
                attempt: current,
                progress,
                ..
            } if *current == attempt && progress != line => {
                *progress = line.to_string();
                (true, None, ())
            }
            _ => (false, None, ()),
        })
    }

    /// Register a prompt of `attempt` and make it visible.  `responder`
    /// is owned here from this call on.  It is refused at once unless
    /// `attempt` is attaching and waits on nothing else.
    pub fn ask(&self, attempt: u64, kind: PromptKind, responder: Responder) {
        self.update(|state| {
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
                    (true, None, ())
                }
                _ => (false, Some((responder, Err(PromptEnded::NotAsking))), ()),
            }
        })
    }

    fn resolve(
        &self,
        attempt: u64,
        prompt: u64,
        outcome: Result<Answer, PromptEnded>,
    ) -> Result<(), AnswerRefused> {
        self.update(|state| {
            let Phase::Attaching {
                attempt: current,
                prompt: pending,
                ..
            } = &mut state.phase
            else {
                return (false, None, Err(AnswerRefused::NoSuchPrompt));
            };
            let Some(asked) = pending
                .as_ref()
                .filter(|p| *current == attempt && p.id == prompt)
            else {
                return (false, None, Err(AnswerRefused::NoSuchPrompt));
            };
            if matches!(&outcome, Ok(answer) if !answer.fits(&asked.kind)) {
                return (false, None, Err(AnswerRefused::WrongKind));
            }
            let responder = pending.take().expect("matched above").responder;
            (true, Some((responder, outcome)), Ok(()))
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

    /// End `attempt`: attached with `windows` mux windows, or failed.  A
    /// prompt still pending is refused.  False when `attempt` is not the
    /// attaching one.
    pub fn finish(&self, attempt: u64, outcome: Result<usize, Failure>) -> bool {
        self.update(|state| {
            if !matches!(state.phase, Phase::Attaching { attempt: current, .. } if current == attempt)
            {
                return (false, None, false);
            }
            let next = match outcome {
                Ok(windows) => Phase::Attached { attempt, windows },
                Err(failure) => Phase::Failed { attempt, failure },
            };
            let Phase::Attaching { prompt, .. } = std::mem::replace(&mut state.phase, next) else {
                unreachable!("checked above");
            };
            let release = prompt.map(|p| (p.responder, Err(PromptEnded::NotAsking)));
            (true, release, true)
        })
    }

    /// The laptop now has `windows` mux windows; ignored unless `attempt`
    /// is attached.
    pub fn windows(&self, attempt: u64, windows: usize) {
        self.update(|state| match &mut state.phase {
            Phase::Attached {
                attempt: current,
                windows: known,
            } if *current == attempt && *known != windows => {
                *known = windows;
                (true, None, ())
            }
            _ => (false, None, ()),
        })
    }

    /// The connection `attempt` attached has ended.
    pub fn detached(&self, attempt: u64) {
        self.update(|state| match state.phase {
            Phase::Attached {
                attempt: current, ..
            } if current == attempt => {
                state.phase = Phase::Disconnected { attempt };
                (true, None, ())
            }
            _ => (false, None, ()),
        })
    }

    /// The GUI engine ended: an attaching attempt fails and its prompt is
    /// refused; an attached connection is disconnected.
    pub fn engine_ended(&self, message: &str) {
        self.update(
            |state| match std::mem::replace(&mut state.phase, Phase::Idle) {
                Phase::Attaching {
                    attempt, prompt, ..
                } => {
                    state.phase = Phase::Failed {
                        attempt,
                        failure: Failure {
                            kind: FailureKind::EngineEnded,
                            message: message.to_string(),
                        },
                    };
                    let release = prompt.map(|p| (p.responder, Err(PromptEnded::NotAsking)));
                    (true, release, ())
                }
                Phase::Attached { attempt, .. } => {
                    state.phase = Phase::Disconnected { attempt };
                    (true, None, ())
                }
                unchanged => {
                    state.phase = unchanged;
                    (false, None, ())
                }
            },
        )
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
            Phase::Failed { attempt, failure } => PhaseView::Failed {
                attempt: *attempt,
                failure: failure.clone(),
            },
            Phase::Disconnected { attempt } => PhaseView::Disconnected { attempt: *attempt },
        };
        Snapshot {
            revision: state.revision,
            phase,
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, channel};

    fn connections() -> Connections {
        Connections::new(|| {})
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

    #[test]
    fn one_connect_operation_at_a_time() {
        let c = connections();
        assert_eq!(c.begin(), Ok(1));
        assert_eq!(c.begin(), Err(Busy));
        assert!(c.finish(1, Ok(2)));
        assert_eq!(c.begin(), Err(Busy));
        c.detached(1);
        assert_eq!(c.begin(), Ok(2));
        assert!(c.finish(2, Err(failure(FailureKind::Authentication))));
        assert_eq!(c.begin(), Ok(3));
    }

    #[test]
    fn a_prompt_is_visible_once_registered_and_takes_one_answer() {
        let c = connections();
        let attempt = c.begin().unwrap();
        let (respond, answered) = responder();
        c.ask(attempt, host_trust(), respond);
        let prompt = pending_prompt(&c).expect("registered");
        assert_eq!(prompt.kind, host_trust());
        assert!(answered.try_recv().is_err());

        assert_eq!(c.answer(attempt, prompt.id, Answer::Trust(true)), Ok(()));
        assert_eq!(answered.recv().unwrap(), Ok(Answer::Trust(true)));
        assert_eq!(pending_prompt(&c), None);
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
        let attempt = c.begin().unwrap();
        let (respond, answered) = responder();
        c.ask(attempt, secret(), respond);
        let prompt = pending_prompt(&c).unwrap().id;

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
        assert_eq!(pending_prompt(&c).map(|p| p.id), Some(prompt));

        assert_eq!(c.answer(attempt, prompt, text()), Ok(()));
        assert_eq!(answered.recv().unwrap(), Ok(text()));
    }

    #[test]
    fn cancelling_resumes_the_asker_with_an_error() {
        let c = connections();
        let attempt = c.begin().unwrap();
        let (respond, answered) = responder();
        c.ask(attempt, secret(), respond);
        let prompt = pending_prompt(&c).unwrap().id;
        assert_eq!(c.cancel(attempt, prompt), Ok(()));
        assert_eq!(answered.recv().unwrap(), Err(PromptEnded::Cancelled));
        assert_eq!(
            c.answer(attempt, prompt, Answer::Text("late".into())),
            Err(AnswerRefused::NoSuchPrompt)
        );
    }

    #[test]
    fn the_end_of_an_attempt_refuses_its_pending_prompt() {
        let c = connections();
        let attempt = c.begin().unwrap();
        let (respond, answered) = responder();
        c.ask(attempt, host_trust(), respond);
        let prompt = pending_prompt(&c).unwrap().id;
        assert!(c.finish(attempt, Err(failure(FailureKind::Unreachable))));
        assert_eq!(answered.recv().unwrap(), Err(PromptEnded::NotAsking));
        assert_eq!(
            c.answer(attempt, prompt, Answer::Trust(true)),
            Err(AnswerRefused::NoSuchPrompt)
        );
        assert_eq!(
            c.snapshot().phase,
            PhaseView::Failed {
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

        let attempt = c.begin().unwrap();
        let (first, first_answered) = responder();
        c.ask(attempt, host_trust(), first);
        let (second, second_answered) = responder();
        c.ask(attempt, secret(), second);
        assert_eq!(second_answered.recv().unwrap(), Err(PromptEnded::NotAsking));
        assert_eq!(pending_prompt(&c).unwrap().kind, host_trust());
        assert!(first_answered.try_recv().is_err());

        assert!(c.finish(attempt, Ok(0)));
        let (late, late_answered) = responder();
        c.ask(attempt, secret(), late);
        assert_eq!(late_answered.recv().unwrap(), Err(PromptEnded::NotAsking));
    }

    #[test]
    fn a_stale_attempt_cannot_finish_or_detach_the_current_one() {
        let c = connections();
        let first = c.begin().unwrap();
        assert!(c.finish(first, Err(failure(FailureKind::Other))));
        let second = c.begin().unwrap();
        assert!(!c.finish(first, Ok(3)));
        assert!(c.finish(second, Ok(0)));
        assert!(!c.finish(second, Err(failure(FailureKind::Other))));
        c.detached(first);
        c.windows(first, 9);
        assert_eq!(
            c.snapshot().phase,
            PhaseView::Attached {
                attempt: second,
                windows: 0
            }
        );
        c.windows(second, 2);
        c.detached(second);
        assert_eq!(
            c.snapshot().phase,
            PhaseView::Disconnected { attempt: second }
        );
    }

    #[test]
    fn the_end_of_the_engine_fails_the_attempt_and_its_prompt() {
        let c = connections();
        let attempt = c.begin().unwrap();
        let (respond, answered) = responder();
        c.ask(attempt, secret(), respond);
        c.engine_ended("GUI thread panicked");
        assert_eq!(answered.recv().unwrap(), Err(PromptEnded::NotAsking));
        assert_eq!(
            c.snapshot().phase,
            PhaseView::Failed {
                attempt,
                failure: Failure {
                    kind: FailureKind::EngineEnded,
                    message: "GUI thread panicked".into()
                }
            }
        );

        let idle = connections();
        idle.engine_ended("stopped");
        assert_eq!(idle.snapshot().phase, PhaseView::Idle);
        assert_eq!(idle.snapshot().revision, 0);
    }

    #[test]
    fn every_change_bumps_the_revision_notifies_and_wakes_waiters() {
        static NOTIFIED: AtomicUsize = AtomicUsize::new(0);
        let c = Arc::new(Connections::new(|| {
            NOTIFIED.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(c.await_change(0, Duration::ZERO), 0);
        let waiter = {
            let c = Arc::clone(&c);
            std::thread::spawn(move || c.await_change(0, Duration::from_secs(60)))
        };
        let attempt = c.begin().unwrap();
        assert_eq!(waiter.join().unwrap(), 1);
        c.progress(attempt, "Connecting");
        c.progress(attempt, "Connecting");
        c.progress(attempt + 1, "stale");
        assert_eq!(c.snapshot().revision, 2);
        assert_eq!(NOTIFIED.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn snapshots_serialize_flat_and_answers_never_print_their_text() {
        let c = connections();
        let attempt = c.begin().unwrap();
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
            })
        );
        assert!(c.finish(attempt, Err(failure(FailureKind::IncompatibleVersion))));
        assert_eq!(
            serde_json::to_value(c.snapshot()).unwrap(),
            serde_json::json!({
                "revision": 3,
                "phase": "failed",
                "attempt": 1,
                "failure": {"kind": "incompatible_version", "message": "why"},
            })
        );
        assert_eq!(
            format!("{:?}", Answer::Text("hunter2".into())),
            "Text(<redacted>)"
        );
    }
}
