//! Android window backend.
//!
//! Kotlin owns the `SurfaceView`; this backend owns the GUI thread, its
//! message loop, the logical windows and the native-window leases.  Surface
//! callbacks arrive as typed [`SurfaceEvent`]s through [`post_surface_event`]
//! and are applied on the GUI thread, so no Java thread ever touches GUI
//! state.
//!
//! Unsafe code is confined to [`window`], which borrows the raw
//! `ANativeWindow` pointer for wgpu.

mod connection;
mod monitor;
mod native_window;
mod window;

pub use connection::{post_surface_event, set_display_dpi, Connection, NotRunning};
pub use monitor::{surface_monitor, SurfaceMonitor, SurfaceSnapshot};
pub use native_window::NativeWindowLease;
pub use window::Window;

/// Surface event with the Android lease type.
pub type AndroidSurfaceEvent = crate::surface::SurfaceEvent<std::sync::Arc<NativeWindowLease>>;

/// A windowing operation that the Android backend does not implement.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("android window backend does not support {operation}")]
pub struct Unsupported {
    /// The `window` contract method that was invoked.
    pub operation: &'static str,
}

fn unsupported<T>(operation: &'static str) -> anyhow::Result<T> {
    Err(Unsupported { operation }.into())
}
