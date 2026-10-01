//! GUI engine ownership and the surface bridge.
//!
//! Java threads call the safe functions here; the GUI thread is the only
//! consumer.  [`EngineGate`] orders every surface callback against the
//! engine's startup, so none is lost or applied out of order.

#![forbid(unsafe_code)]

use crate::engine::{EngineGate, NotAccepting};
use crate::{InitOutcome, InitRequest};
use ndk::native_window::NativeWindow;
use serde::Serialize;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};
use thiserror::Error;
use wezterm_gui::renderfault::{RenderFailures, RenderStage};
use window::os::android::{
    AndroidSurfaceEvent, NativeWindowLease, SurfaceSnapshot, post_surface_event, surface_monitor,
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

fn fail(stage: EngineStage, message: String) {
    log::error!("GUI engine failed at {stage:?}: {message}");
    ENGINE.fail(stage, message);
}

fn deliver(event: AndroidSurfaceEvent) {
    if let Err(err) = post_surface_event(event) {
        log::error!("surface event lost: {err}");
    }
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
        fail(EngineStage::Gui, format!("spawn GUI thread: {err}"));
    }
    engine_state()
}

/// Current engine state.
pub fn engine_state() -> EngineState {
    ENGINE.state()
}

fn gui_thread(request: StartRequest) {
    let init = crate::initialize(&request.init);
    if let InitOutcome::Failed { stage, message } = init.outcome {
        fail(EngineStage::Init, format!("{stage:?}: {message}"));
        return;
    }

    let mut overrides = vec![("check_for_updates".to_string(), "false".to_string())];
    overrides.extend(request.config_overrides);
    if let Err(err) = config::set_config_overrides(&overrides) {
        fail(EngineStage::ConfigOverrides, format!("{err:#}"));
        return;
    }
    config::reload();

    let options = wezterm_gui::android::GuiOptions {
        dpi: request.init.dpi as usize,
        diagnostic_applet: request.diagnostic_applet,
    };
    let run = std::panic::AssertUnwindSafe(|| run_gui(options));
    match std::panic::catch_unwind(run) {
        Ok(Ok(())) => ENGINE.stop(),
        Ok(Err(err)) => fail(EngineStage::Gui, format!("{err:#}")),
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            fail(EngineStage::Gui, format!("GUI thread panicked: {message}"));
        }
    }
}

fn run_gui(options: wezterm_gui::android::GuiOptions) -> anyhow::Result<()> {
    wezterm_gui::android::run(options, |ready| match ready {
        Ok(()) => {
            let thread = format!("{:?}", std::thread::current().id());
            let replayed = ENGINE.publish_running(thread, deliver);
            log::info!("GUI engine running; replayed {replayed} queued surface event(s)");
        }
        Err(err) => fail(EngineStage::Gui, format!("{err:#}")),
    })
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

fn post(event: AndroidSurfaceEvent) -> Result<(), SurfaceBridgeError> {
    Ok(ENGINE.post(event, deliver)?)
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
    post(SurfaceEvent::Created {
        generation,
        lease: NativeWindowLease::new(generation, window),
        geometry: SurfaceGeometry::new(width, height),
    })
}

/// `surfaceChanged`.
pub fn surface_changed(
    generation_raw: i64,
    width: i32,
    height: i32,
) -> Result<(), SurfaceBridgeError> {
    post(SurfaceEvent::Changed {
        generation: generation(generation_raw)?,
        geometry: SurfaceGeometry::new(width, height),
    })
}

/// How `surface_destroyed` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireOutcome {
    /// The native window of that generation was released, or the engine
    /// never held it.
    Acknowledged,
    /// No acknowledgment arrived: the GUI thread failed while the event was
    /// in flight.  Its state, and any lease it held, was not cleaned up.
    Dropped,
}

/// `surfaceDestroyed`: block the caller until every GPU reference to the
/// surface and the native window lease are gone (or, for a generation the
/// GUI thread does not hold, until it says so).
///
/// The wait cannot deadlock on the caller: the GUI thread never calls into
/// Java, and the acknowledgment is sent from `NativeWindowLease::drop`,
/// which runs on whichever thread drops the last lease clone.
pub fn surface_destroyed(generation_raw: i64) -> Result<RetireOutcome, SurfaceBridgeError> {
    let generation = generation(generation_raw)?;
    let (tx, rx) = channel();
    post(SurfaceEvent::Destroyed {
        generation,
        ack: RetireAck::new(tx),
    })?;
    let started = Instant::now();
    let outcome = match rx.recv() {
        Ok(()) => RetireOutcome::Acknowledged,
        Err(_) => RetireOutcome::Dropped,
    };
    log::info!(
        "surface generation {} destroy returned {outcome:?} after {:?}",
        generation.get(),
        started.elapsed()
    );
    Ok(outcome)
}

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
