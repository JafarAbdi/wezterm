//! Android window backend.  Every windowing operation reports
//! [`Unsupported`] until the native-window lease, looper wakeups and wgpu
//! surface exist; nothing here creates a window or starts a message loop.

use crate::connection::ConnectionOps;
use crate::{Clipboard, CursorIcon, RequestedWindowGeometry, WindowEvent, WindowOps};
use async_trait::async_trait;
use config::ConfigHandle;
use promise::Future;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use std::any::Any;
use std::rc::Rc;
use thiserror::Error;
use wezterm_font::FontConfiguration;

/// A windowing operation that the Android backend does not implement yet.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("android window backend does not support {operation} yet")]
pub struct Unsupported {
    /// The `window` contract method that was invoked.
    pub operation: &'static str,
}

fn unsupported<T>(operation: &'static str) -> anyhow::Result<T> {
    Err(Unsupported { operation }.into())
}

/// Process-wide windowing connection.  There is no display server to
/// connect to; the Kotlin side owns Android surfaces.
pub struct Connection {}

/// Logical native window identity.  Stable across Android surface
/// changes once later stages attach surfaces to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Window(usize);

impl Connection {
    pub(crate) fn create_new() -> anyhow::Result<Connection> {
        Ok(Connection {})
    }

    pub async fn new_window<F>(
        &self,
        _class_name: &str,
        _name: &str,
        _geometry: RequestedWindowGeometry,
        _config: Option<&ConfigHandle>,
        _font_config: Rc<FontConfiguration>,
        _event_handler: F,
    ) -> anyhow::Result<Window>
    where
        F: 'static + FnMut(WindowEvent, &Window),
    {
        unsupported("Connection::new_window")
    }
}

impl ConnectionOps for Connection {
    fn name(&self) -> String {
        "Android".to_string()
    }

    fn terminate_message_loop(&self) {}

    fn run_message_loop(&self) -> anyhow::Result<()> {
        unsupported("Connection::run_message_loop")
    }
}

impl Window {
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
        let conn = Connection::get().ok_or_else(|| Unsupported {
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
        Err(HandleError::Unavailable)
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

#[async_trait(?Send)]
impl WindowOps for Window {
    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>> {
        unsupported("WindowOps::enable_opengl")
    }

    fn show(&self) {}

    fn notify<T: Any + Send + Sync>(&self, _t: T)
    where
        Self: Sized,
    {
    }

    fn hide(&self) {}

    fn close(&self) {}

    fn set_cursor(&self, _cursor: Option<CursorIcon>) {}

    fn invalidate(&self) {}

    fn set_title(&self, _title: &str) {}

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
}
