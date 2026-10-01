//! Reference-counted ownership of one `ANativeWindow`.

#![forbid(unsafe_code)]

use crate::surface::{RetireAck, SurfaceGeneration};
use ndk::native_window::NativeWindow;
use raw_window_handle::{AndroidNdkWindowHandle, RawWindowHandle};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

static LIVE_LEASES: AtomicUsize = AtomicUsize::new(0);

/// One acquired `ANativeWindow` reference for one surface generation.
///
/// The backend holds it in its surface state and hands clones to GPU state
/// through [`crate::SurfaceLease`].  The native reference is released when
/// the last clone drops; if a retirement was armed by then, the platform
/// thread waiting on it is released immediately after.
pub struct NativeWindowLease {
    generation: SurfaceGeneration,
    window: Option<NativeWindow>,
    retire: Mutex<Option<RetireAck>>,
}

impl NativeWindowLease {
    pub fn new(generation: SurfaceGeneration, window: NativeWindow) -> Arc<Self> {
        LIVE_LEASES.fetch_add(1, Ordering::SeqCst);
        Arc::new(Self {
            generation,
            window: Some(window),
            retire: Mutex::new(None),
        })
    }

    pub fn generation(&self) -> SurfaceGeneration {
        self.generation
    }

    /// The pointer wgpu targets.  Valid while any clone of this lease lives.
    pub fn raw_window_handle(&self) -> RawWindowHandle {
        let window = self.window.as_ref().expect("window is taken only in Drop");
        RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(window.ptr().cast()))
    }

    /// Acknowledge `ack` once the native reference is released.
    pub fn arm_retire(&self, ack: RetireAck) {
        self.retire.lock().unwrap().replace(ack);
    }

    /// Leases whose native reference is still held, process-wide.
    pub fn live_count() -> usize {
        LIVE_LEASES.load(Ordering::SeqCst)
    }
}

impl Drop for NativeWindowLease {
    fn drop(&mut self) {
        drop(self.window.take());
        LIVE_LEASES.fetch_sub(1, Ordering::SeqCst);
        log::info!(
            "surface generation {} native window released",
            self.generation.get()
        );
        let ack = self.retire.lock().unwrap().take();
        super::monitor::surface_monitor().update(|s| s.retire_acks += u64::from(ack.is_some()));
        if let Some(ack) = ack {
            ack.send();
        }
    }
}

impl std::fmt::Debug for NativeWindowLease {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("NativeWindowLease")
            .field("generation", &self.generation.get())
            .finish()
    }
}
