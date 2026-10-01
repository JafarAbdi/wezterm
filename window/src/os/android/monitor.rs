//! Process-wide, thread-safe view of the surface lifecycle for diagnostics
//! and for platform threads that must wait on a GUI-thread condition.

#![forbid(unsafe_code)]

use super::native_window::NativeWindowLease;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

static LOOP_WAKEUPS: AtomicU64 = AtomicU64::new(0);

pub(super) fn note_loop_wakeup() {
    LOOP_WAKEUPS.fetch_add(1, Ordering::Relaxed);
}

/// What the window selector shows for one logical window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowSummary {
    pub id: usize,
    pub title: String,
}

/// Counters the GUI thread updates; every change wakes waiters.
#[derive(Debug, Clone, Serialize)]
pub struct SurfaceSnapshot {
    /// Number of updates so far; `await_change` waits for it to move.
    pub revision: u64,
    /// `absent`, `unsized` or `present`.
    pub state: &'static str,
    /// Generation of the current surface, if any.
    pub generation: Option<u64>,
    /// Drawable size while `present`.
    pub width: u32,
    pub height: u32,
    /// Frames presented on the current generation.
    pub frames_presented: u64,
    /// Frames presented since process start.
    pub total_frames_presented: u64,
    /// Callbacks ignored because their generation was not current.
    pub stale_events: u64,
    /// Retirements acknowledged after their native reference was released.
    pub retire_acks: u64,
    /// Native window references still held by any lease.
    pub live_leases: usize,
    /// The logical window that presents on the surface slot.
    pub bound_window: Option<usize>,
    /// Every logical window, bound or surfaceless, by ascending id.
    pub windows: Vec<WindowSummary>,
    /// Logical windows closed since process start.
    pub closed_windows: u64,
    /// Clipboard reads asked of the platform, and reads it answered.
    pub clipboard_requests: u64,
    pub clipboard_responses: u64,
    /// Times the message loop woke from `poll()`.
    pub loop_wakeups: u64,
}

impl Default for SurfaceSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            state: "absent",
            generation: None,
            width: 0,
            height: 0,
            frames_presented: 0,
            total_frames_presented: 0,
            stale_events: 0,
            retire_acks: 0,
            live_leases: 0,
            bound_window: None,
            windows: Vec::new(),
            closed_windows: 0,
            clipboard_requests: 0,
            clipboard_responses: 0,
            loop_wakeups: 0,
        }
    }
}

pub struct SurfaceMonitor {
    snapshot: Mutex<SurfaceSnapshot>,
    changed: Condvar,
}

static MONITOR: OnceLock<SurfaceMonitor> = OnceLock::new();

pub fn surface_monitor() -> &'static SurfaceMonitor {
    MONITOR.get_or_init(|| SurfaceMonitor {
        snapshot: Mutex::new(SurfaceSnapshot::default()),
        changed: Condvar::new(),
    })
}

impl SurfaceMonitor {
    pub fn update(&self, f: impl FnOnce(&mut SurfaceSnapshot)) {
        let mut snapshot = self.snapshot.lock().unwrap();
        f(&mut snapshot);
        snapshot.revision += 1;
        self.changed.notify_all();
    }

    pub fn snapshot(&self) -> SurfaceSnapshot {
        let mut snapshot = self.snapshot.lock().unwrap().clone();
        snapshot.live_leases = NativeWindowLease::live_count();
        snapshot.loop_wakeups = LOOP_WAKEUPS.load(Ordering::Relaxed);
        snapshot
    }

    /// Block until at least `min_frames` were presented on `generation`, or
    /// `timeout` elapses.  Returns whether the condition was met.
    pub fn await_frames(&self, generation: u64, min_frames: u64, timeout: Duration) -> bool {
        self.await_condition(timeout, |s| {
            s.generation == Some(generation) && s.frames_presented >= min_frames
        })
    }

    /// Block until the slot is in `state` (for `generation`, or any
    /// generation when `None`), or `timeout` elapses.
    pub fn await_state(&self, state: &str, generation: Option<u64>, timeout: Duration) -> bool {
        self.await_condition(timeout, |s| {
            s.state == state && generation.map_or(true, |g| s.generation == Some(g))
        })
    }

    /// Block until the revision differs from `since`, or `timeout` elapses.
    /// Returns the revision then current.
    pub fn await_change(&self, since: u64, timeout: Duration) -> u64 {
        let guard = self.snapshot.lock().unwrap();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |s| s.revision == since)
            .unwrap();
        guard.revision
    }

    fn await_condition(&self, timeout: Duration, done: impl Fn(&SurfaceSnapshot) -> bool) -> bool {
        let guard = self.snapshot.lock().unwrap();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |s| !done(s))
            .unwrap();
        done(&guard)
    }
}
