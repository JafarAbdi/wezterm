//! Android window backend.
//!
//! Kotlin owns the `SurfaceView`; this backend owns the GUI thread, its
//! message loop, the logical windows and the native-window leases.  The
//! GUI thread applies platform events through [`Connection`] and asks the
//! platform for things through [`platform_requests`]; no Java thread ever
//! touches GUI state and the GUI thread never calls into Java.
//!
//! Unsafe code is confined to [`window`], which borrows the raw
//! `ANativeWindow` pointer for wgpu.

mod connection;
mod monitor;
mod native_window;
mod requests;
mod window;

pub use connection::{on_rebind, set_display_dpi, Connection};
pub use monitor::{surface_monitor, InputCounts, SurfaceMonitor, SurfaceSnapshot, WindowSummary};
pub use native_window::NativeWindowLease;
pub use requests::{platform_requests, ClipboardUnavailable, PlatformRequest, PlatformRequests};
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
