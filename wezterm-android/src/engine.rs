//! Mailbox and lifecycle of the GUI engine: the phase of the one GUI
//! thread per process and every platform event it has not applied yet.
//!
//! Kotlin threads post typed [`PlatformEvent`]s; the GUI thread takes them
//! one at a time, in arrival order, once it is running.  The queue belongs
//! to the gate, never to the GUI thread's task queue, so an engine that ends
//! can still dispose of what it never applied: [`EngineGate::shut`] stops
//! accepting, lets the GUI thread retire what it holds, releases every
//! queued lease and only then resolves every queued destroy.
//!
//! The gate is host-testable; the Android bridge (`terminal`) instantiates
//! it with the native-window lease.

#![forbid(unsafe_code)]

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use thiserror::Error;
use window::surface::SurfaceEvent;

/// What the platform tells the GUI thread.
#[derive(Debug)]
pub enum PlatformEvent<L> {
    /// A `SurfaceHolder` callback.
    Surface(SurfaceEvent<L>),
    /// The user picked the logical window with this id in the selector.
    SelectWindow(usize),
    /// Kotlin answered clipboard read `request`; `None` when the platform
    /// refused the read.
    ClipboardText {
        /// The id the request carried.
        request: u64,
        /// The clipboard's text.
        text: Option<String>,
    },
}

/// Which bootstrap step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineStage {
    /// `crate::initialize` (paths, config, fonts).
    Init,
    /// Applying config overrides.
    ConfigOverrides,
    /// Window connection, mux or frontend creation, or the message loop.
    Gui,
}

/// Snapshot of the GUI thread's lifecycle, as reported to Java.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EngineState {
    /// `start` has not been called.
    NotStarted,
    /// The GUI thread is bootstrapping; `queued` events wait.
    Starting {
        /// Events held until the engine runs.
        queued: usize,
    },
    /// The GUI thread accepts work.
    Running {
        /// Thread id of the GUI thread.
        thread: String,
    },
    /// Bootstrap or the message loop failed.
    Failed {
        /// The step that failed.
        stage: EngineStage,
        /// Human-readable error chain.
        message: String,
    },
    /// The message loop ended.
    Stopped,
}

/// How the engine ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEnd {
    /// Bootstrap, the message loop or a GUI-thread panic.
    Failed {
        /// The step that failed.
        stage: EngineStage,
        /// Human-readable error chain.
        message: String,
    },
    /// The message loop returned.
    Stopped,
}

enum Phase<L> {
    NotStarted,
    Starting {
        pending: VecDeque<PlatformEvent<L>>,
    },
    Running {
        thread: String,
        pending: VecDeque<PlatformEvent<L>>,
    },
    /// No event is accepted.  `retired` turns true once the shutdown has
    /// disposed of everything the engine held or had queued.
    Ended {
        end: EngineEnd,
        retired: bool,
    },
}

impl<L> Phase<L> {
    fn state(&self) -> EngineState {
        match self {
            Self::NotStarted => EngineState::NotStarted,
            Self::Starting { pending } => EngineState::Starting {
                queued: pending.len(),
            },
            Self::Running { thread, .. } => EngineState::Running {
                thread: thread.clone(),
            },
            Self::Ended {
                end: EngineEnd::Failed { stage, message },
                ..
            } => EngineState::Failed {
                stage: *stage,
                message: message.clone(),
            },
            Self::Ended {
                end: EngineEnd::Stopped,
                ..
            } => EngineState::Stopped,
        }
    }
}

/// The engine refused an event; the event was dropped, which releases a
/// created lease and closes a destroy's acknowledgment.
#[derive(Debug, Error)]
#[error("the GUI engine is not accepting platform events: {0:?}")]
pub struct NotAccepting(
    /// The state that refused the event.
    pub EngineState,
);

/// Phase of the GUI thread and the platform events it has not applied yet.
pub struct EngineGate<L> {
    phase: Mutex<Phase<L>>,
    changed: Condvar,
}

impl<L> EngineGate<L> {
    /// A gate in `NotStarted`.
    pub const fn new() -> Self {
        Self {
            phase: Mutex::new(Phase::NotStarted),
            changed: Condvar::new(),
        }
    }

    /// Snapshot of the current phase.
    pub fn state(&self) -> EngineState {
        self.phase.lock().unwrap().state()
    }

    /// `NotStarted` to `Starting`; otherwise the current state, unchanged.
    pub fn begin(&self) -> Result<(), EngineState> {
        let mut phase = self.phase.lock().unwrap();
        match *phase {
            Phase::NotStarted => {
                *phase = Phase::Starting {
                    pending: VecDeque::new(),
                };
                Ok(())
            }
            _ => Err(phase.state()),
        }
    }

    /// Queue `event`.  While the engine runs, `wake` tells the GUI thread
    /// to call [`Self::next`]; while it starts, the event waits for
    /// [`Self::publish_running`].
    pub fn post(&self, event: PlatformEvent<L>, wake: impl FnOnce()) -> Result<(), NotAccepting> {
        let mut phase = self.phase.lock().unwrap();
        match &mut *phase {
            Phase::Starting { pending } => pending.push_back(event),
            Phase::Running { pending, .. } => {
                pending.push_back(event);
                wake();
            }
            Phase::NotStarted | Phase::Ended { .. } => return Err(NotAccepting(phase.state())),
        }
        self.changed.notify_all();
        Ok(())
    }

    /// `Starting` to `Running`, keeping the queue.  Returns how many events
    /// wait for [`Self::next`]; `None` unless the engine was starting.
    pub fn publish_running(&self, thread: String) -> Option<usize> {
        let mut phase = self.phase.lock().unwrap();
        let Phase::Starting { pending } = &mut *phase else {
            return None;
        };
        let pending = std::mem::take(pending);
        let queued = pending.len();
        *phase = Phase::Running { thread, pending };
        Some(queued)
    }

    /// The oldest queued event, for the GUI thread to apply.  `None` when
    /// the queue is empty or the engine is not running.
    pub fn next(&self) -> Option<PlatformEvent<L>> {
        match &mut *self.phase.lock().unwrap() {
            Phase::Running { pending, .. } => pending.pop_front(),
            _ => None,
        }
    }

    /// End the engine, in this order: stop accepting events; run `retire`,
    /// in which the GUI thread drops every GPU reference and the lease it
    /// holds and reports whether nothing of a native window survived;
    /// release every queued lease; resolve every queued destroy.  A destroy
    /// is acknowledged when `retire` succeeded and dropped otherwise, which
    /// its waiter reads as "not released".  Waiters of
    /// [`Self::await_retired`] wake last.
    ///
    /// Only the first call has an effect.
    pub fn shut(&self, end: EngineEnd, retire: impl FnOnce() -> bool) {
        let pending = {
            let mut phase = self.phase.lock().unwrap();
            let ended = Phase::Ended {
                end,
                retired: false,
            };
            match std::mem::replace(&mut *phase, ended) {
                Phase::NotStarted => VecDeque::new(),
                Phase::Starting { pending } | Phase::Running { pending, .. } => pending,
                first @ Phase::Ended { .. } => {
                    *phase = first;
                    return;
                }
            }
        };
        let released = retire();
        let mut acks = Vec::new();
        for event in pending {
            match event {
                PlatformEvent::Surface(SurfaceEvent::Created { lease, .. }) => drop(lease),
                PlatformEvent::Surface(SurfaceEvent::Destroyed { ack, .. }) => acks.push(ack),
                PlatformEvent::Surface(SurfaceEvent::Changed { .. })
                | PlatformEvent::SelectWindow(_)
                | PlatformEvent::ClipboardText { .. } => {}
            }
        }
        if released {
            for ack in acks {
                ack.send();
            }
        } else {
            drop(acks);
        }
        if let Phase::Ended { retired, .. } = &mut *self.phase.lock().unwrap() {
            *retired = true;
        }
        self.changed.notify_all();
    }

    /// Block while a started engine has not finished [`Self::shut`].
    ///
    /// For a platform thread whose destroy acknowledgment was dropped:
    /// that only happens while the GUI thread unwinds or after the engine
    /// ended, and in both cases `shut` follows.
    pub fn await_retired(&self) {
        let phase = self.phase.lock().unwrap();
        let _retired = self
            .changed
            .wait_while(phase, |phase| match phase {
                Phase::NotStarted => false,
                Phase::Starting { .. } | Phase::Running { .. } => true,
                Phase::Ended { retired, .. } => !*retired,
            })
            .unwrap();
    }

    /// Block until an event matching `wanted` is queued; false after
    /// `timeout`.  Debug fault injection uses it to fail the GUI thread
    /// while a destroy is still queued.
    #[cfg(any(test, debug_assertions))]
    pub fn await_queued(
        &self,
        wanted: impl Fn(&PlatformEvent<L>) -> bool,
        timeout: std::time::Duration,
    ) -> bool {
        let queued = |phase: &Phase<L>| match phase {
            Phase::Starting { pending } | Phase::Running { pending, .. } => {
                pending.iter().any(&wanted)
            }
            Phase::NotStarted | Phase::Ended { .. } => false,
        };
        let phase = self.phase.lock().unwrap();
        let (phase, _) = self
            .changed
            .wait_timeout_while(phase, timeout, |phase| !queued(phase))
            .unwrap();
        queued(&phase)
    }
}

impl<L> Default for EngineGate<L> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, TryRecvError, channel};
    use std::time::Duration;
    use window::surface::{
        RetireAck, SurfaceEffect, SurfaceGeneration, SurfaceGeometry, SurfaceState,
    };

    fn generation(n: u64) -> SurfaceGeneration {
        SurfaceGeneration::new(n).unwrap()
    }

    fn geom(w: i32, h: i32) -> Option<SurfaceGeometry> {
        Some(SurfaceGeometry::new(w, h).unwrap())
    }

    fn ack() -> (RetireAck, Receiver<()>) {
        let (tx, rx) = channel();
        (RetireAck::new(tx), rx)
    }

    fn created<L>(n: u64, lease: L, geometry: Option<SurfaceGeometry>) -> PlatformEvent<L> {
        PlatformEvent::Surface(SurfaceEvent::Created {
            generation: generation(n),
            lease,
            geometry,
        })
    }

    fn changed<L>(n: u64, geometry: Option<SurfaceGeometry>) -> PlatformEvent<L> {
        PlatformEvent::Surface(SurfaceEvent::Changed {
            generation: generation(n),
            geometry,
        })
    }

    fn destroyed<L>(n: u64, ack: RetireAck) -> PlatformEvent<L> {
        PlatformEvent::Surface(SurfaceEvent::Destroyed {
            generation: generation(n),
            ack,
        })
    }

    fn name<L>(event: &PlatformEvent<L>) -> String {
        match event {
            PlatformEvent::Surface(SurfaceEvent::Created { generation, .. }) => {
                format!("created:{}", generation.get())
            }
            PlatformEvent::Surface(SurfaceEvent::Changed { generation, .. }) => {
                format!("changed:{}", generation.get())
            }
            PlatformEvent::Surface(SurfaceEvent::Destroyed { generation, .. }) => {
                format!("destroyed:{}", generation.get())
            }
            PlatformEvent::SelectWindow(id) => format!("select:{id}"),
            PlatformEvent::ClipboardText { request, .. } => format!("clipboard:{request}"),
        }
    }

    fn drain<L>(gate: &EngineGate<L>) -> Vec<PlatformEvent<L>> {
        std::iter::from_fn(|| gate.next()).collect()
    }

    fn effect_name(effect: &SurfaceEffect<&str>) -> String {
        match effect {
            SurfaceEffect::Available(g) => format!("available:{}x{}", g.width(), g.height()),
            SurfaceEffect::Resized(g) => format!("resized:{}x{}", g.width(), g.height()),
            SurfaceEffect::Lost => "lost".into(),
            SurfaceEffect::Retire { lease, ack } => format!(
                "retire:{lease}:{}",
                if ack.is_some() { "ack" } else { "silent" }
            ),
            SurfaceEffect::Ack(_) => "ack".into(),
            SurfaceEffect::Stale(g) => format!("stale:{}", g.get()),
        }
    }

    type Log = Arc<Mutex<Vec<String>>>;

    fn record(log: &Log, entry: impl Into<String>) {
        log.lock().unwrap().push(entry.into());
    }

    /// Host stand-in for a native window lease that records its release.
    #[derive(Debug)]
    struct Lease {
        name: &'static str,
        log: Log,
    }

    impl Drop for Lease {
        fn drop(&mut self) {
            record(&self.log, format!("released:{}", self.name));
        }
    }

    fn lease(log: &Log, name: &'static str) -> Lease {
        Lease {
            name,
            log: Arc::clone(log),
        }
    }

    /// A platform thread blocked in `surfaceDestroyed`: it records how the
    /// acknowledgment resolved and, like the bridge, waits for the shutdown
    /// when the acknowledgment was dropped.
    fn destroy_waiter(
        gate: &'static EngineGate<Lease>,
        log: &Log,
        rx: Receiver<()>,
    ) -> std::thread::JoinHandle<()> {
        let log = Arc::clone(log);
        std::thread::spawn(move || match rx.recv() {
            Ok(()) => record(&log, "acknowledged"),
            Err(_) => {
                gate.await_retired();
                record(&log, "dropped");
            }
        })
    }

    fn leaked_gate() -> &'static EngineGate<Lease> {
        Box::leak(Box::new(EngineGate::new()))
    }

    fn failed(message: &str) -> EngineEnd {
        EngineEnd::Failed {
            stage: EngineStage::Gui,
            message: message.into(),
        }
    }

    #[test]
    fn events_queued_while_starting_are_applied_before_a_destroy_posted_while_running() {
        let gate = EngineGate::<&str>::new();
        let wakes = std::cell::Cell::new(0);
        let wake = || wakes.set(wakes.get() + 1);
        gate.begin().unwrap();
        gate.post(created(1, "lease1", geom(100, 200)), wake)
            .unwrap();
        gate.post(changed(1, geom(100, 200)), wake).unwrap();
        assert_eq!(gate.state(), EngineState::Starting { queued: 2 });
        assert!(gate.next().is_none(), "nothing is applied while starting");
        assert_eq!(wakes.get(), 0, "a starting engine has no thread to wake");

        assert_eq!(gate.publish_running("gui".into()), Some(2));
        assert_eq!(
            gate.state(),
            EngineState::Running {
                thread: "gui".into()
            }
        );
        let (retire_ack, rx) = ack();
        gate.post(destroyed(1, retire_ack), wake).unwrap();
        assert_eq!(wakes.get(), 1);

        let events = drain(&gate);
        assert_eq!(
            events.iter().map(name).collect::<Vec<_>>(),
            ["created:1", "changed:1", "destroyed:1"]
        );

        let mut state = SurfaceState::default();
        let mut effects = Vec::new();
        for event in events {
            let PlatformEvent::Surface(event) = event else {
                unreachable!()
            };
            let (next, applied) = state.apply(event);
            effects.extend(applied);
            state = next;
        }
        assert_eq!(
            effects.iter().map(effect_name).collect::<Vec<_>>(),
            ["available:100x200", "lost", "retire:lease1:ack"],
            "the lease retires together with the ack; nothing acks before the release"
        );
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "the platform thread stays blocked until the lease is released"
        );
        assert_eq!(state.high_water(), Some(generation(1)));
    }

    #[test]
    fn failure_while_starting_releases_queued_leases_before_acknowledging_queued_destroys() {
        let log = Log::default();
        let gate = leaked_gate();
        gate.begin().unwrap();
        gate.post(created(1, lease(&log, "lease1"), None), || {})
            .unwrap();
        let (retire_ack, rx) = ack();
        gate.post(destroyed(1, retire_ack), || {}).unwrap();
        let waiter = destroy_waiter(gate, &log, rx);

        gate.shut(failed("boom"), || true);
        waiter.join().unwrap();
        assert_eq!(*log.lock().unwrap(), ["released:lease1", "acknowledged"]);
        assert_eq!(
            gate.state(),
            EngineState::Failed {
                stage: EngineStage::Gui,
                message: "boom".into()
            }
        );
    }

    #[test]
    fn failure_while_running_retires_the_held_lease_then_queued_leases_then_queued_destroys() {
        let log = Log::default();
        let gate = leaked_gate();
        gate.begin().unwrap();
        gate.publish_running("gui".into()).unwrap();
        // The GUI thread holds generation 1 and dies before it applies the
        // destroy of 1, the creation of 2 and the destroy of 2.
        let held = lease(&log, "held1");
        let (ack1, rx1) = ack();
        let (ack2, rx2) = ack();
        gate.post(destroyed(1, ack1), || {}).unwrap();
        gate.post(created(2, lease(&log, "queued2"), None), || {})
            .unwrap();
        gate.post(PlatformEvent::SelectWindow(7), || {}).unwrap();
        gate.post(destroyed(2, ack2), || {}).unwrap();
        let waiters = [
            destroy_waiter(gate, &log, rx1),
            destroy_waiter(gate, &log, rx2),
        ];

        gate.shut(failed("GUI thread panicked"), || {
            let (late_ack, _late_rx) = ack();
            let refused = gate.post(destroyed(3, late_ack), || {}).unwrap_err();
            assert!(matches!(refused.0, EngineState::Failed { .. }));
            record(&log, "refused:destroyed:3");
            drop(held);
            true
        });
        for waiter in waiters {
            waiter.join().unwrap();
        }
        assert_eq!(
            *log.lock().unwrap(),
            [
                "refused:destroyed:3",
                "released:held1",
                "released:queued2",
                "acknowledged",
                "acknowledged"
            ]
        );
        assert!(gate.next().is_none(), "an ended engine applies nothing");
    }

    #[test]
    fn destroy_whose_surface_could_not_be_released_is_dropped_and_returns_after_the_shutdown() {
        let log = Log::default();
        let gate = leaked_gate();
        gate.begin().unwrap();
        gate.publish_running("gui".into()).unwrap();
        let (queued_ack, queued_rx) = ack();
        gate.post(destroyed(1, queued_ack), || {}).unwrap();
        let queued = destroy_waiter(gate, &log, queued_rx);

        // A destroy the GUI thread had taken when it panicked: unwinding
        // drops its acknowledgment before the shutdown starts.
        let (taken_ack, taken_rx) = ack();
        gate.post(destroyed(1, taken_ack), || {}).unwrap();
        let first = gate.next().unwrap();
        let taken = gate.next().unwrap();
        gate.post(first, || {}).unwrap();
        let in_flight = destroy_waiter(gate, &log, taken_rx);
        drop(taken);

        gate.shut(failed("GUI thread panicked"), || {
            record(&log, "retire:failed");
            false
        });
        queued.join().unwrap();
        in_flight.join().unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            ["retire:failed", "dropped", "dropped"],
            "no waiter returns before the shutdown ran, and none is told the surface was released"
        );
    }

    #[test]
    fn refused_events_release_their_lease_and_only_the_first_shutdown_counts() {
        let log = Log::default();
        let gate = leaked_gate();
        assert_eq!(gate.state(), EngineState::NotStarted);
        assert!(
            gate.post(created(1, lease(&log, "early"), None), || {})
                .is_err()
        );
        gate.await_retired();

        gate.begin().unwrap();
        assert_eq!(gate.begin(), Err(EngineState::Starting { queued: 0 }));
        assert_eq!(gate.publish_running("gui".into()), Some(0));
        assert_eq!(gate.publish_running("again".into()), None);

        gate.shut(EngineEnd::Stopped, || true);
        gate.shut(failed("late"), || {
            panic!("a second shutdown must not retire")
        });
        assert_eq!(gate.state(), EngineState::Stopped);
        assert!(
            gate.post(created(2, lease(&log, "late"), None), || {})
                .is_err()
        );
        gate.await_retired();
        assert_eq!(*log.lock().unwrap(), ["released:early", "released:late"]);
    }

    #[test]
    fn await_queued_sees_a_destroy_posted_by_another_thread() {
        let gate = leaked_gate();
        gate.begin().unwrap();
        gate.publish_running("gui".into()).unwrap();
        let is_destroy = |event: &PlatformEvent<Lease>| {
            matches!(
                event,
                PlatformEvent::Surface(SurfaceEvent::Destroyed { .. })
            )
        };
        assert!(!gate.await_queued(is_destroy, Duration::ZERO));
        let poster = std::thread::spawn(move || {
            let (retire_ack, _rx) = ack();
            gate.post(destroyed(1, retire_ack), || {}).unwrap();
        });
        assert!(gate.await_queued(is_destroy, Duration::from_secs(30)));
        poster.join().unwrap();
    }
}
