//! Counters of GPU failures on a presentation surface, the wait that lets
//! platform threads observe them, and the debug-only injection that drives
//! the recovery paths under test.

use serde::Serialize;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Where a surface's GPU work can fail.  The codes are the fault kinds
/// `NativeApp.nativeDiagnosticFault` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RenderStage {
    /// Building `WebGpuState` and `RenderState` for a new surface.
    GpuCreation = 2,
    /// Drawing one frame.
    Draw = 3,
}

impl RenderStage {
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            2 => Some(Self::GpuCreation),
            3 => Some(Self::Draw),
            _ => None,
        }
    }
}

/// Failures since process start.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct RenderFailures {
    pub gpu_creation: u64,
    pub draw: u64,
}

impl RenderFailures {
    fn count(&self, stage: RenderStage) -> u64 {
        match stage {
            RenderStage::GpuCreation => self.gpu_creation,
            RenderStage::Draw => self.draw,
        }
    }

    fn count_mut(&mut self, stage: RenderStage) -> &mut u64 {
        match stage {
            RenderStage::GpuCreation => &mut self.gpu_creation,
            RenderStage::Draw => &mut self.draw,
        }
    }
}

static FAILURES: Mutex<RenderFailures> = Mutex::new(RenderFailures {
    gpu_creation: 0,
    draw: 0,
});
static CHANGED: Condvar = Condvar::new();
static ARMED: AtomicU8 = AtomicU8::new(0);

/// Make the next attempt at `stage` fail once.
#[cfg(debug_assertions)]
pub fn arm(stage: RenderStage) {
    ARMED.store(stage as u8, Ordering::SeqCst);
}

/// The armed failure for `stage`, consumed.
pub(crate) fn injected(stage: RenderStage) -> Option<anyhow::Error> {
    ARMED
        .compare_exchange(stage as u8, 0, Ordering::SeqCst, Ordering::SeqCst)
        .ok()
        .map(|_| anyhow::anyhow!("injected {stage:?} failure"))
}

pub(crate) fn record(stage: RenderStage) {
    *FAILURES.lock().unwrap().count_mut(stage) += 1;
    CHANGED.notify_all();
}

pub fn failures() -> RenderFailures {
    *FAILURES.lock().unwrap()
}

/// Block until at least `min` failures of `stage` were recorded, or
/// `timeout` elapses.  Returns whether the condition was met.
pub fn await_failures(stage: RenderStage, min: u64, timeout: Duration) -> bool {
    let guard = FAILURES.lock().unwrap();
    let (guard, _) = CHANGED
        .wait_timeout_while(guard, timeout, |f| f.count(stage) < min)
        .unwrap();
    guard.count(stage) >= min
}
