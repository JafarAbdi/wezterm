//! Logical window handle.  A `Window` is an id; its state lives in the
//! `Connection` on the GUI thread and every operation is routed there.

use super::connection::Connection;
use super::monitor::surface_monitor;
use super::{unsupported, Unsupported};
use crate::connection::ConnectionOps;
use crate::{
    Clipboard, CursorIcon, RequestedWindowGeometry, SurfaceLease, WindowEvent, WindowEventSender,
    WindowOps,
};
use async_trait::async_trait;
use config::ConfigHandle;
use promise::Future;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use std::any::Any;
use std::rc::Rc;
use wezterm_font::FontConfiguration;

/// Logical window identity.  Stable across surface changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Window(usize);

pub(super) struct WindowInner {
    pub events: WindowEventSender,
    pub invalidated: bool,
}

impl WindowInner {
    pub fn new(events: WindowEventSender) -> Self {
        Self {
            events,
            invalidated: false,
        }
    }
}

impl Window {
    pub(super) fn new(id: usize) -> Self {
        Self(id)
    }

    pub async fn new_window<F>(
        class_name: &str,
        name: &str,
        geometry: RequestedWindowGeometry,
        config: Option<&ConfigHandle>,
        font_config: Rc<FontConfiguration>,
        event_handler: F,
    ) -> anyhow::Result<Window>
    where
        F: 'static + FnMut(WindowEvent, &Window),
    {
        let conn = Connection::get().ok_or(Unsupported {
            operation: "Window::new_window without Connection::init",
        })?;
        conn.new_window(
            class_name,
            name,
            geometry,
            config,
            font_config,
            event_handler,
        )
        .await
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::android())
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let lease = Connection::get()
            .and_then(|conn| conn.surface_lease_for(self.0))
            .ok_or(HandleError::Unavailable)?;
        let raw = lease.raw_window_handle();
        // SAFETY: `raw` is the `ANativeWindow` owned by `lease`, which holds
        // an acquired reference for as long as any clone of it lives.  The
        // GPU state that consumes this handle keeps such a clone through
        // `WindowOps::surface_lease`, so the pointer outlives every use of
        // the returned handle.  Handles are only produced on the GUI thread
        // while the window is bound to a present surface.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

#[async_trait(?Send)]
impl WindowOps for Window {
    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>> {
        unsupported("WindowOps::enable_opengl")
    }

    fn show(&self) {}

    fn notify<T: Any + Send + Sync>(&self, t: T)
    where
        Self: Sized,
    {
        Connection::with_window_inner(self.0, move |inner| {
            inner
                .events
                .dispatch(WindowEvent::Notification(Box::new(t)));
        });
    }

    fn hide(&self) {}

    fn close(&self) {
        let id = self.0;
        promise::spawn::spawn_into_main_thread(async move {
            if let Some(conn) = Connection::get() {
                conn.close_window(id);
            }
        })
        .detach();
    }

    fn set_cursor(&self, _cursor: Option<CursorIcon>) {}

    fn invalidate(&self) {
        Connection::with_window_inner(self.0, |inner| inner.invalidated = true);
    }

    fn set_title(&self, title: &str) {
        log::debug!("window {} title: {title}", self.0);
    }

    fn set_inner_size(&self, _width: usize, _height: usize) {}

    fn get_clipboard(&self, _clipboard: Clipboard) -> Future<String> {
        Future::err(
            Unsupported {
                operation: "WindowOps::get_clipboard",
            }
            .into(),
        )
    }

    fn set_clipboard(&self, _clipboard: Clipboard, _text: String) {}

    fn surface_lease(&self) -> Option<SurfaceLease> {
        let lease = Connection::get()?.surface_lease_for(self.0)?;
        Some(SurfaceLease::new(lease))
    }

    fn frame_presented(&self) {
        surface_monitor().update(|s| {
            s.frames_presented += 1;
            s.total_frames_presented += 1;
            if s.frames_presented == 1 {
                log::info!(
                    "surface generation {} first frame presented ({}x{})",
                    s.generation.unwrap_or(0),
                    s.width,
                    s.height
                );
            }
        });
    }
}
