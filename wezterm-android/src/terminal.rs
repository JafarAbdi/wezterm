//! GUI engine ownership and the platform bridge.
//!
//! Java threads call the safe functions here; the GUI thread is the only
//! consumer.  [`EngineGate`] holds every platform event until the GUI
//! thread applies it, in arrival order, and disposes of the rest when the
//! engine ends.

#![forbid(unsafe_code)]

use crate::engine::{EngineEnd, EngineGate, NotAccepting, PlatformEvent};
use crate::{InitOutcome, InitRequest};
use ndk::native_window::NativeWindow;
use serde::Serialize;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};
use thiserror::Error;
use wezterm_gui::renderfault::{RenderFailures, RenderStage};
use window::ConnectionOps;
use window::os::android::{
    Connection, NativeWindowLease, PlatformRequest, SurfaceSnapshot, platform_requests,
    surface_monitor,
};
use window::surface::{RetireAck, SurfaceEvent, SurfaceGeneration, SurfaceGeometry};

pub use crate::engine::{EngineStage, EngineState};

/// What `start` needs.
#[derive(Debug, Clone)]
pub struct StartRequest {
    /// Paths, density and logging, as for `initialize`.
    pub init: InitRequest,
    /// Debug builds only: open the diagnostic applet window.
    pub diagnostic_applet: bool,
    /// Extra `key=value` config overrides (Lua expressions), debug only.
    pub config_overrides: Vec<(String, String)>,
}

static ENGINE: EngineGate<Arc<NativeWindowLease>> = EngineGate::new();

type Event = PlatformEvent<Arc<NativeWindowLease>>;

/// End the engine from the GUI thread (or from the thread that failed to
/// spawn it): retire what the window backend holds, then everything queued.
fn shut(end: EngineEnd) {
    match &end {
        EngineEnd::Failed { stage, message } => {
            log::error!("GUI engine failed at {stage:?}: {message}");
        }
        EngineEnd::Stopped => log::info!("GUI engine stopped"),
    }
    ENGINE.shut(end, || {
        platform_requests().close();
        // A window handler that panics here leaves its GPU state behind;
        // the lease it holds is then reported as not released.
        let retired = std::panic::catch_unwind(|| Connection::get().is_none_or(|c| c.retire()));
        let released = retired.unwrap_or(false);
        log::info!(
            "GUI engine retired its surface (released={released}, live leases {})",
            NativeWindowLease::live_count()
        );
        released
    });
}

fn failed(stage: EngineStage, message: String) -> EngineEnd {
    EngineEnd::Failed { stage, message }
}

fn drain() {
    while let Some(event) = ENGINE.next() {
        let Some(conn) = Connection::get() else {
            log::error!("platform event {event:?} arrived without a Connection");
            continue;
        };
        match event {
            PlatformEvent::Surface(event) => conn.apply_surface_event(event),
            PlatformEvent::SelectWindow(id) => conn.select_window(id),
            PlatformEvent::ClipboardText { request, text } => {
                platform_requests().complete_clipboard(request, text);
            }
        }
    }
}

fn wake() {
    promise::spawn::spawn_into_main_thread(async { drain() }).detach();
}

fn post(event: Event) -> Result<(), NotAccepting> {
    ENGINE.post(event, wake)
}

/// Start the GUI thread once per process; later calls report the current
/// state without side effects.
pub fn start(request: StartRequest) -> EngineState {
    if let Err(state) = ENGINE.begin() {
        return state;
    }
    if let Err(err) = std::thread::Builder::new()
        .name("wezterm-gui".into())
        .spawn(move || gui_thread(request))
    {
        shut(failed(EngineStage::Gui, format!("spawn GUI thread: {err}")));
    }
    engine_state()
}

/// Current engine state.
pub fn engine_state() -> EngineState {
    ENGINE.state()
}

fn gui_thread(request: StartRequest) {
    let run = std::panic::AssertUnwindSafe(|| run_gui(request));
    let end = match std::panic::catch_unwind(run) {
        Ok(Ok(())) => EngineEnd::Stopped,
        Ok(Err(end)) => end,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            failed(EngineStage::Gui, format!("GUI thread panicked: {message}"))
        }
    };
    let stopped = end == EngineEnd::Stopped;
    shut(end);
    if !stopped {
        // The GUI's thread-local object graph is in whatever state the
        // failure left it.  Its destructors would run when this thread
        // exits, and one that panics aborts the process (observed: the
        // frontend's windows).  Keep the thread, and the graph, parked.
        loop {
            std::thread::park();
        }
    }
}

fn run_gui(request: StartRequest) -> Result<(), EngineEnd> {
    let init = crate::initialize(&request.init);
    if let InitOutcome::Failed { stage, message } = init.outcome {
        return Err(failed(EngineStage::Init, format!("{stage:?}: {message}")));
    }

    let mut overrides = vec![("check_for_updates".to_string(), "false".to_string())];
    overrides.extend(request.config_overrides);
    config::set_config_overrides(&overrides)
        .map_err(|err| failed(EngineStage::ConfigOverrides, format!("{err:#}")))?;
    config::reload();

    let options = wezterm_gui::android::GuiOptions {
        dpi: request.init.dpi as usize,
        diagnostic_applet: request.diagnostic_applet,
    };
    wezterm_gui::android::run(options, || {
        let thread = format!("{:?}", std::thread::current().id());
        let queued = ENGINE.publish_running(thread);
        log::info!("GUI engine running; {queued:?} platform event(s) were queued");
        wake();
    })
    .map_err(|err| failed(EngineStage::Gui, format!("{err:#}")))
}

/// Why a surface callback could not be accepted.
#[derive(Debug, Error)]
pub enum SurfaceBridgeError {
    /// Java passed a generation that is not positive.
    #[error("surface generation must be positive, got {0}")]
    InvalidGeneration(i64),
    /// `ANativeWindow_fromSurface` returned null.
    #[error("android.view.Surface has no native window")]
    NoNativeWindow,
    /// The engine is not accepting surface events.
    #[error(transparent)]
    NotAccepting(#[from] NotAccepting),
}

fn generation(raw: i64) -> Result<SurfaceGeneration, SurfaceBridgeError> {
    u64::try_from(raw)
        .ok()
        .and_then(SurfaceGeneration::new)
        .ok_or(SurfaceBridgeError::InvalidGeneration(raw))
}

/// `surfaceCreated`: take ownership of the acquired native window.
pub fn surface_created(
    generation_raw: i64,
    window: Option<NativeWindow>,
    width: i32,
    height: i32,
) -> Result<(), SurfaceBridgeError> {
    let generation = generation(generation_raw)?;
    let window = window.ok_or(SurfaceBridgeError::NoNativeWindow)?;
    Ok(post(PlatformEvent::Surface(SurfaceEvent::Created {
        generation,
        lease: NativeWindowLease::new(generation, window),
        geometry: SurfaceGeometry::new(width, height),
    }))?)
}

/// `surfaceChanged`.
pub fn surface_changed(
    generation_raw: i64,
    width: i32,
    height: i32,
) -> Result<(), SurfaceBridgeError> {
    Ok(post(PlatformEvent::Surface(SurfaceEvent::Changed {
        generation: generation(generation_raw)?,
        geometry: SurfaceGeometry::new(width, height),
    }))?)
}

/// How `surface_destroyed` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireOutcome {
    /// The native window of that generation was released, or the engine
    /// never held it.
    Acknowledged,
    /// The engine ended.  Its shutdown has finished, so nothing touches
    /// the surface any more, but it did not confirm the release: either it
    /// held nothing of that generation or GPU state could not be dropped.
    Dropped,
}

/// `surfaceDestroyed`: block the caller until every GPU reference to the
/// surface and the native window lease are gone (or, for a generation the
/// GUI thread does not hold, until it says so).
///
/// The wait cannot deadlock on the caller: the GUI thread never calls into
/// Java, and the acknowledgment is sent from `NativeWindowLease::drop`,
/// which runs on whichever thread drops the last lease clone.  It cannot
/// outlive the GUI thread either: an acknowledgment that thread dropped,
/// or an engine that refuses the event, is followed by the engine's
/// shutdown, and the caller waits only for that.
pub fn surface_destroyed(generation_raw: i64) -> Result<RetireOutcome, SurfaceBridgeError> {
    let generation = generation(generation_raw)?;
    let (tx, rx) = channel();
    let started = Instant::now();
    let posted = post(PlatformEvent::Surface(SurfaceEvent::Destroyed {
        generation,
        ack: RetireAck::new(tx),
    }));
    let outcome = match posted.map(|()| rx.recv()) {
        Ok(Ok(())) => RetireOutcome::Acknowledged,
        Ok(Err(_)) | Err(_) => {
            ENGINE.await_retired();
            RetireOutcome::Dropped
        }
    };
    log::info!(
        "surface generation {} destroy returned {outcome:?} after {:?}",
        generation.get(),
        started.elapsed()
    );
    Ok(outcome)
}

/// The user picked logical window `id` in the selector.
pub fn select_window(id: i64) -> Result<(), NotAccepting> {
    // An id that is not a window id selects nothing, like any unknown id.
    post(PlatformEvent::SelectWindow(
        usize::try_from(id).unwrap_or(0),
    ))
}

/// Kotlin answered clipboard read `request`.
pub fn clipboard_text(request: i64, text: Option<String>) -> Result<(), NotAccepting> {
    post(PlatformEvent::ClipboardText {
        request: u64::try_from(request).unwrap_or(0),
        text,
    })
}

/// Block until the GUI thread asks the platform for something; `None`
/// once the engine ended.
pub fn next_request() -> Option<PlatformRequest> {
    platform_requests().next()
}

/// What debug builds can make the GUI thread do.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticCommand {
    /// Open another diagnostic applet window.
    OpenWindow,
    /// Paste the clipboard into the bound window's active pane.
    Paste,
    /// Hold the GUI thread until a surface destroy is queued behind it,
    /// then panic.
    PanicOnQueuedDestroy,
    /// Start a clipboard read on the bound window and panic in the same
    /// GUI-thread task; reports whether the read then failed.
    PanicWithClipboardRead,
    /// Make the next `SurfaceLost` handler panic before it drops GPU state.
    PanicInSurfaceLost,
}

#[cfg(debug_assertions)]
impl DiagnosticCommand {
    /// Parse the name Java passes.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "open-window" => Some(Self::OpenWindow),
            "paste" => Some(Self::Paste),
            "panic-on-queued-destroy" => Some(Self::PanicOnQueuedDestroy),
            "panic-with-clipboard-read" => Some(Self::PanicWithClipboardRead),
            "panic-in-surface-lost" => Some(Self::PanicInSurfaceLost),
            _ => None,
        }
    }
}

/// Run `command` on the GUI thread.  False when no GUI thread accepts
/// work; for `PanicWithClipboardRead`, also when the read did not fail.
#[cfg(debug_assertions)]
pub fn diagnostic(command: DiagnosticCommand) -> bool {
    use wezterm_gui::android::diagnostic;
    if !matches!(engine_state(), EngineState::Running { .. }) {
        return false;
    }
    match command {
        DiagnosticCommand::OpenWindow => on_gui_thread(diagnostic::open_window),
        DiagnosticCommand::Paste => on_gui_thread(|| {
            if let Err(err) = diagnostic::paste_into_bound_window() {
                log::error!("diagnostic paste: {err:#}");
            }
        }),
        DiagnosticCommand::PanicOnQueuedDestroy => on_gui_thread(|| {
            let is_destroy = |event: &Event| {
                matches!(
                    event,
                    PlatformEvent::Surface(SurfaceEvent::Destroyed { .. })
                )
            };
            if ENGINE.await_queued(is_destroy, DIAGNOSTIC_WAIT) {
                panic!("diagnostic GUI-thread panic with a surface destroy queued");
            }
            log::error!("no surface destroy was queued; not panicking");
        }),
        DiagnosticCommand::PanicWithClipboardRead => return clipboard_read_fails_with_the_engine(),
        DiagnosticCommand::PanicInSurfaceLost => wezterm_gui::renderfault::arm_surface_lost_panic(),
    }
    true
}

#[cfg(debug_assertions)]
fn on_gui_thread(task: impl FnOnce() + Send + 'static) {
    promise::spawn::spawn_into_main_thread(async move { task() }).detach();
}

/// The GUI thread starts a clipboard read through the bound window and
/// panics before it runs another task.  Blocks the caller until the read's
/// future resolves on a thread of its own; true when it resolved with an
/// error.
#[cfg(debug_assertions)]
fn clipboard_read_fails_with_the_engine() -> bool {
    let (started_tx, started_rx) = channel();
    on_gui_thread(
        move || match wezterm_gui::android::diagnostic::read_bound_clipboard() {
            Ok(read) => {
                started_tx.send(read).ok();
                panic!("diagnostic GUI-thread panic with a clipboard read pending");
            }
            Err(err) => log::error!("diagnostic clipboard read: {err:#}"),
        },
    );
    let Ok(read) = started_rx.recv() else {
        return false;
    };
    let (outcome_tx, outcome_rx) = channel();
    std::thread::spawn(move || outcome_tx.send(promise::spawn::block_on(read)).ok());
    match outcome_rx.recv_timeout(DIAGNOSTIC_WAIT) {
        Ok(Err(err)) => {
            log::info!("diagnostic clipboard read failed with the engine: {err:#}");
            true
        }
        Ok(Ok(_)) => {
            log::error!("diagnostic clipboard read returned text from a failed engine");
            false
        }
        Err(_) => {
            log::error!("diagnostic clipboard read is still pending after {DIAGNOSTIC_WAIT:?}");
            false
        }
    }
}

/// How long a diagnostic waits for the condition its test produces.  Test
/// scaffolding, not a product timeout.
#[cfg(debug_assertions)]
const DIAGNOSTIC_WAIT: Duration = Duration::from_secs(60);

/// Engine and surface state for diagnostics.
#[derive(Debug, Serialize)]
pub struct Status {
    /// GUI thread lifecycle.
    pub engine: EngineState,
    /// Surface slot counters.
    pub surface: SurfaceSnapshot,
    /// GPU failures the bound window survived.
    pub render: RenderFailures,
}

/// Snapshot of engine and surface state.
pub fn status() -> Status {
    Status {
        engine: engine_state(),
        surface: surface_monitor().snapshot(),
        render: wezterm_gui::renderfault::failures(),
    }
}

/// Block until the surface status changes from revision `since`; returns
/// the revision then current.
pub fn await_change(since: i64, timeout_ms: i64) -> i64 {
    surface_monitor().await_change(
        u64::try_from(since).unwrap_or(0),
        Duration::from_millis(timeout_ms.max(0) as u64),
    ) as i64
}

/// Block until `min_frames` frames were presented on `generation`.
pub fn await_frames(generation_raw: i64, min_frames: i64, timeout_ms: i64) -> bool {
    let Ok(generation) = generation(generation_raw) else {
        return false;
    };
    surface_monitor().await_frames(
        generation.get(),
        min_frames.max(0) as u64,
        Duration::from_millis(timeout_ms.max(0) as u64),
    )
}

/// Block until `min_failures` GPU failures of the stage coded `stage_raw`
/// were recorded; false at once for an unknown stage.
pub fn await_render_failures(stage_raw: i32, min_failures: i64, timeout_ms: i64) -> bool {
    let Some(stage) = RenderStage::from_code(stage_raw) else {
        return false;
    };
    wezterm_gui::renderfault::await_failures(
        stage,
        min_failures.max(0) as u64,
        Duration::from_millis(timeout_ms.max(0) as u64),
    )
}

/// Block until the surface slot is in `state`; `generation_raw <= 0`
/// accepts any generation.
pub fn await_state(state: &str, generation_raw: i64, timeout_ms: i64) -> bool {
    surface_monitor().await_state(
        state,
        generation(generation_raw).ok().map(|g| g.get()),
        Duration::from_millis(timeout_ms.max(0) as u64),
    )
}
