//! Process-wide, thread-safe view of the surface lifecycle for diagnostics
//! and for platform threads that must wait on a GUI-thread condition.

#![forbid(unsafe_code)]

use super::native_window::NativeWindowLease;
use super::requests::{platform_requests, PlatformRequest};
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
    /// The cursor cell the bound window last painted, in surface pixels:
    /// its origin and the cell size.  All zero before it painted.
    pub cursor_x: i64,
    pub cursor_y: i64,
    pub cell_width: u32,
    pub cell_height: u32,
    /// Platform input dispatched to the bound window, by kind.
    pub input: InputCounts,
    /// The local pane id the bound window's keyboard input reaches; `None`
    /// while it shows no pane.
    pub input_pane: Option<usize>,
    /// Changes of `input_pane`, and of the bound window that may have
    /// moved it away and back, since process start.  An IME edit that
    /// erases text it typed carries the generation it typed under; the GUI
    /// thread refuses it under any other, also when an earlier pane is
    /// the target again.
    pub input_generation: u64,
}

/// Platform input the GUI thread applied, by kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InputCounts {
    /// Composing-text updates; these stay on the phone.
    pub preedits: u64,
    /// IME commits.
    pub commits: u64,
    /// Key presses.
    pub keys: u64,
    /// Paste requests.
    pub pastes: u64,
    /// Touch gestures.
    pub touches: u64,
    /// Input that arrived while no window was bound, and was dropped.
    pub dropped: u64,
    /// IME edits refused because they would erase text in a pane that is
    /// no longer the one they typed into.
    pub refused: u64,
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
            cursor_x: 0,
            cursor_y: 0,
            cell_width: 0,
            cell_height: 0,
            input: InputCounts::default(),
            input_pane: None,
            input_generation: 0,
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

    /// Record the bound window's cursor cell; an unchanged cell wakes nobody.
    pub(super) fn set_text_cursor(&self, x: i64, y: i64, width: u32, height: u32) {
        let cell = (x, y, width, height);
        let unchanged = {
            let s = self.snapshot.lock().unwrap();
            (s.cursor_x, s.cursor_y, s.cell_width, s.cell_height) == cell
        };
        if !unchanged {
            self.update(|s| (s.cursor_x, s.cursor_y, s.cell_width, s.cell_height) = cell);
        }
    }

    /// Record the pane the bound window's input reaches now and return its
    /// generation.  Another pane, or a change that may have moved the
    /// input away and back (`moved`), starts a new generation, and the
    /// platform is told.
    pub fn observe_input_pane(&self, pane: Option<usize>, moved: bool) -> u64 {
        let mut snapshot = self.snapshot.lock().unwrap();
        if snapshot.input_pane == pane && !moved {
            return snapshot.input_generation;
        }
        snapshot.input_pane = pane;
        snapshot.input_generation += 1;
        snapshot.revision += 1;
        let generation = snapshot.input_generation;
        drop(snapshot);
        self.changed.notify_all();
        platform_requests().push(PlatformRequest::WindowsChanged);
        generation
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
