//! Startup gate of the GUI engine: the phase of the one GUI thread per
//! process, with the surface callbacks that arrive before it accepts work.
//!
//! The gate is host-testable; the Android bridge (`terminal`) instantiates
//! it with the native-window lease and delivers accepted events to the GUI
//! thread's queue.  Every callback is delivered exactly once and in arrival
//! order: callbacks that arrive while the engine is starting queue, the
//! handoff delivers the queue before it publishes `Running`, and a callback
//! that arrives during the handoff queues behind what is already there.

#![forbid(unsafe_code)]

use serde::Serialize;
use std::sync::Mutex;
use thiserror::Error;
use window::surface::SurfaceEvent;

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
    /// The GUI thread is bootstrapping; `queued` surface callbacks wait.
    Starting {
        /// Surface callbacks held for the handoff.
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

enum Phase<L> {
    NotStarted,
    Starting { pending: Vec<SurfaceEvent<L>> },
    Running { thread: String },
    Failed { stage: EngineStage, message: String },
    Stopped,
}

impl<L> Phase<L> {
    fn state(&self) -> EngineState {
        match self {
            Self::NotStarted => EngineState::NotStarted,
            Self::Starting { pending } => EngineState::Starting {
                queued: pending.len(),
            },
            Self::Running { thread } => EngineState::Running {
                thread: thread.clone(),
            },
            Self::Failed { stage, message } => EngineState::Failed {
                stage: *stage,
                message: message.clone(),
            },
            Self::Stopped => EngineState::Stopped,
        }
    }

    fn replace(&mut self, next: Self) -> Vec<SurfaceEvent<L>> {
        match std::mem::replace(self, next) {
            Self::Starting { pending } => pending,
            _ => Vec::new(),
        }
    }
}

/// The engine refused a surface callback; the event was dropped, which
/// releases a created lease and closes a destroy's acknowledgment.
#[derive(Debug, Error)]
#[error("the GUI engine is not accepting surface events: {0:?}")]
pub struct NotAccepting(
    /// The state that refused the event.
    pub EngineState,
);

/// Phase of the GUI thread and the surface callbacks it has not accepted yet.
pub struct EngineGate<L> {
    phase: Mutex<Phase<L>>,
}

impl<L> EngineGate<L> {
    /// A gate in `NotStarted`.
    pub const fn new() -> Self {
        Self {
            phase: Mutex::new(Phase::NotStarted),
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
                    pending: Vec::new(),
                };
                Ok(())
            }
            _ => Err(phase.state()),
        }
    }

    /// Queue `event` while starting or hand it to `deliver` while running.
    pub fn post(
        &self,
        event: SurfaceEvent<L>,
        deliver: impl FnOnce(SurfaceEvent<L>),
    ) -> Result<(), NotAccepting> {
        let mut phase = self.phase.lock().unwrap();
        match &mut *phase {
            Phase::Starting { pending } => {
                pending.push(event);
                Ok(())
            }
            Phase::Running { .. } => {
                deliver(event);
                Ok(())
            }
            Phase::NotStarted | Phase::Failed { .. } | Phase::Stopped => {
                Err(NotAccepting(phase.state()))
            }
        }
    }

    /// Deliver every queued callback in arrival order, then publish
    /// `Running`.  A callback posted while a batch is being delivered joins
    /// the queue and is delivered before `Running` is published, so nothing
    /// overtakes the queue.  Returns how many callbacks were delivered.
    /// Does nothing unless the engine is starting.
    pub fn publish_running(
        &self,
        mut thread: String,
        mut deliver: impl FnMut(SurfaceEvent<L>),
    ) -> usize {
        let mut delivered = 0;
        loop {
            let batch = {
                let mut phase = self.phase.lock().unwrap();
                match &mut *phase {
                    Phase::Starting { pending } if pending.is_empty() => {
                        *phase = Phase::Running {
                            thread: std::mem::take(&mut thread),
                        };
                        return delivered;
                    }
                    Phase::Starting { pending } => std::mem::take(pending),
                    _ => return delivered,
                }
            };
            delivered += batch.len();
            for event in batch {
                deliver(event);
            }
        }
    }

    /// The engine failed.  Queued callbacks are abandoned: every queued
    /// lease is released first, then every queued destroy is acknowledged.
    pub fn fail(&self, stage: EngineStage, message: String) {
        let pending = self
            .phase
            .lock()
            .unwrap()
            .replace(Phase::Failed { stage, message });
        abandon(pending);
    }

    /// The message loop ended.
    pub fn stop(&self) {
        let pending = self.phase.lock().unwrap().replace(Phase::Stopped);
        abandon(pending);
    }
}

impl<L> Default for EngineGate<L> {
    fn default() -> Self {
        Self::new()
    }
}

fn abandon<L>(pending: Vec<SurfaceEvent<L>>) {
    let mut acks = Vec::new();
    for event in pending {
        match event {
            SurfaceEvent::Created { lease, .. } => drop(lease),
            SurfaceEvent::Changed { .. } => {}
            SurfaceEvent::Destroyed { ack, .. } => acks.push(ack),
        }
    }
    for ack in acks {
        ack.send();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, TryRecvError, channel};
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

    fn created<L>(n: u64, lease: L, geometry: Option<SurfaceGeometry>) -> SurfaceEvent<L> {
        SurfaceEvent::Created {
            generation: generation(n),
            lease,
            geometry,
        }
    }

    fn changed<L>(n: u64, geometry: Option<SurfaceGeometry>) -> SurfaceEvent<L> {
        SurfaceEvent::Changed {
            generation: generation(n),
            geometry,
        }
    }

    fn destroyed<L>(n: u64, ack: RetireAck) -> SurfaceEvent<L> {
        SurfaceEvent::Destroyed {
            generation: generation(n),
            ack,
        }
    }

    fn event_names<L>(events: &[SurfaceEvent<L>]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                SurfaceEvent::Created { generation, .. } => format!("created:{}", generation.get()),
                SurfaceEvent::Changed { generation, .. } => format!("changed:{}", generation.get()),
                SurfaceEvent::Destroyed { generation, .. } => {
                    format!("destroyed:{}", generation.get())
                }
            })
            .collect()
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

    /// Host stand-in for a native window lease that records its release.
    #[derive(Debug)]
    struct Lease {
        name: &'static str,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Drop for Lease {
        fn drop(&mut self) {
            self.log
                .lock()
                .unwrap()
                .push(format!("released:{}", self.name));
        }
    }

    #[test]
    fn creation_queued_while_starting_is_delivered_before_a_destroy_that_arrives_during_handoff() {
        let gate = EngineGate::<&str>::new();
        let delivered: RefCell<Vec<SurfaceEvent<&'static str>>> = RefCell::new(Vec::new());
        let record = |event: SurfaceEvent<&'static str>| delivered.borrow_mut().push(event);
        gate.begin().unwrap();
        gate.post(created(1, "lease1", geom(100, 200)), record)
            .unwrap();
        gate.post(changed(1, geom(100, 200)), record).unwrap();
        assert_eq!(gate.state(), EngineState::Starting { queued: 2 });
        assert!(delivered.borrow().is_empty());

        let (retire_ack, rx) = ack();
        let mut destroy = Some(destroyed(1, retire_ack));
        let replayed = gate.publish_running("gui".into(), |event| {
            record(event);
            // The platform destroys the surface while the GUI thread is
            // still handing the queue over.
            if let Some(destroy) = destroy.take() {
                gate.post(destroy, record).unwrap();
            }
        });
        assert_eq!(
            event_names(&delivered.borrow()),
            ["created:1", "changed:1", "destroyed:1"]
        );
        assert_eq!(replayed, 3);
        assert_eq!(
            gate.state(),
            EngineState::Running {
                thread: "gui".into()
            }
        );

        let mut state = SurfaceState::default();
        let mut effects = Vec::new();
        for event in delivered.take() {
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

        let (state, replay) = state.apply(created(1, "lease1-replayed", geom(100, 200)));
        assert_eq!(
            replay.iter().map(effect_name).collect::<Vec<_>>(),
            ["stale:1"]
        );
        assert_eq!(state.lease(), None);
    }

    #[test]
    fn failure_while_starting_releases_queued_leases_before_acknowledging_queued_destroys() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let lease = |name| Lease {
            name,
            log: Arc::clone(&log),
        };
        let never = |event: SurfaceEvent<Lease>| panic!("{event:?} delivered while starting");
        let gate = EngineGate::<Lease>::new();
        gate.begin().unwrap();
        gate.post(created(1, lease("lease1"), None), never).unwrap();
        let (retire_ack, rx) = ack();
        gate.post(destroyed(1, retire_ack), never).unwrap();
        let waiter = {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                rx.recv()
                    .expect("the queued destroy is acknowledged, not dropped");
                log.lock().unwrap().push("acked:1".into());
            })
        };

        gate.fail(EngineStage::Gui, "boom".into());
        waiter.join().unwrap();
        assert_eq!(*log.lock().unwrap(), ["released:lease1", "acked:1"]);
        assert_eq!(
            gate.state(),
            EngineState::Failed {
                stage: EngineStage::Gui,
                message: "boom".into()
            }
        );

        let refused = gate
            .post(created(2, lease("lease2"), None), never)
            .unwrap_err();
        assert!(matches!(refused.0, EngineState::Failed { .. }));
        assert_eq!(
            *log.lock().unwrap(),
            ["released:lease1", "acked:1", "released:lease2"]
        );
    }

    #[test]
    fn running_engine_delivers_directly_and_begins_once() {
        let gate = EngineGate::<&str>::new();
        let delivered: RefCell<Vec<SurfaceEvent<&'static str>>> = RefCell::new(Vec::new());
        let record = |event: SurfaceEvent<&'static str>| delivered.borrow_mut().push(event);
        assert_eq!(gate.state(), EngineState::NotStarted);
        assert!(gate.post(changed(1, None), record).is_err());

        gate.begin().unwrap();
        assert_eq!(gate.begin(), Err(EngineState::Starting { queued: 0 }));
        assert_eq!(gate.publish_running("gui".into(), record), 0);
        gate.post(changed(1, geom(1, 1)), record).unwrap();
        assert_eq!(event_names(&delivered.borrow()), ["changed:1"]);

        gate.stop();
        assert_eq!(gate.state(), EngineState::Stopped);
        assert!(gate.post(changed(1, None), record).is_err());
        assert_eq!(event_names(&delivered.borrow()), ["changed:1"]);
    }
}
