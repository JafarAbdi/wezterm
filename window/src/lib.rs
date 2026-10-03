use async_trait::async_trait;
use bitflags::bitflags;
use config::window::WindowLevel;
use config::{ConfigHandle, Dimension, GeometryOrigin};
use promise::Future;
use std::any::Any;
use std::path::PathBuf;
use std::rc::Rc;
use thiserror::Error;
use url::Url;
pub mod bitmaps;
pub use wezterm_color_types as color;
mod configuration;
pub mod connection;
pub mod os;
pub mod screen;
mod spawn;
pub mod surface;

pub use cursor_icon::CursorIcon;
pub use raw_window_handle;

#[cfg(target_os = "macos")]
pub(crate) const DEFAULT_DPI: f64 = 72.0;
#[cfg(not(target_os = "macos"))]
pub(crate) const DEFAULT_DPI: f64 = 96.0;

pub fn default_dpi() -> f64 {
    match Connection::get() {
        Some(conn) => conn.default_dpi(),
        None => DEFAULT_DPI,
    }
}

#[cfg(not(target_os = "android"))]
mod egl;

pub use bitmaps::{BitmapImage, Image};
pub use connection::*;
pub use glium;
pub use os::*;
pub use wezterm_input_types::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clipboard {
    Clipboard,
    PrimarySelection,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::Clipboard
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: usize,
}

pub type ULength = euclid::Length<usize, PixelUnit>;
pub type Rect = euclid::Rect<isize, PixelUnit>;
pub type RectF = euclid::Rect<f32, PixelUnit>;
pub type Size = euclid::Size2D<isize, PixelUnit>;
pub type SizeF = euclid::Size2D<f32, PixelUnit>;
pub type ScreenRect = euclid::Rect<isize, ScreenPixelUnit>;

/// Represents the preferred appearance of the windowing
/// environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Appearance {
    /// Standard dark-text-on-light-background presentation
    Light,
    /// Dark mode, with predominantly dark or muted colors
    Dark,
    /// dark-text-on-light-background, but in a higher contrast
    /// more accesible palette
    LightHighContrast,
    /// darker background but with higher contrast than regular
    /// dark mode
    DarkHighContrast,
}

impl std::string::ToString for Appearance {
    fn to_string(&self) -> String {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::LightHighContrast => "LightHighContrast",
            Self::DarkHighContrast => "DarkHighContrast",
        }
        .to_string()
    }
}

bitflags! {
    #[derive(Default)]
    pub struct WindowState: u8 {
        /// Occupies the whole screen; cannot be resized while in this state.
        const FULL_SCREEN = 1<<1;
        /// Maximized along either or both of horizontal or vertical dimensions;
        /// cannot be resized while in this state.
        const MAXIMIZED = 1<<2;
        /// Minimized or in some kind of off-screen state. Cannot be repainted
        /// while in this state.
        const HIDDEN = 1<<3;
        /// Always on top (floating) window
        const ALWAYS_ON_TOP = 1<<4;
        /// Always on bottom (docked) window
        const ALWAYS_ON_BOTTOM = 1<<5;
    }
}

impl WindowState {
    pub fn can_resize(self) -> bool {
        !self.intersects(Self::FULL_SCREEN | Self::MAXIMIZED)
    }

    pub fn can_paint(self) -> bool {
        !self.contains(Self::HIDDEN)
    }

    pub fn as_window_level(self) -> WindowLevel {
        if self.contains(Self::ALWAYS_ON_TOP) {
            WindowLevel::AlwaysOnTop
        } else if self.contains(Self::ALWAYS_ON_BOTTOM) {
            WindowLevel::AlwaysOnBottom
        } else {
            WindowLevel::Normal
        }
    }
}

#[derive(Debug, Clone)]
pub enum WindowKeyEvent {
    RawKeyEvent(RawKeyEvent),
    KeyEvent(KeyEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadKeyStatus {
    /// Not in a dead key processing hold
    None,
    /// Holding until composition is done; the string is the uncommitted
    /// composition text to show as a placeholder
    Composing(String),
}

#[derive(Debug)]
pub enum WindowEvent {
    /// Called when the window close button is clicked.
    /// The window closure is deferred and this event is
    /// sent to your application to decide whether it will
    /// really close the window.
    CloseRequested,

    /// Called when the window is being destroyed by the window system
    Destroyed,

    /// A presentation surface for the window became available.  Backends
    /// whose surfaces arrive after the window exists (Android) send this
    /// before the matching `Resized`; GPU state may be created now.
    SurfaceAvailable {
        dimensions: Dimensions,
    },

    /// The presentation surface is going away.  Every GPU reference to it
    /// must be dropped before the handler returns.  The logical window
    /// stays alive and a later `SurfaceAvailable` resumes rendering; this
    /// is neither `CloseRequested` nor `Destroyed`.
    SurfaceLost,

    /// Called when the window has been resized
    Resized {
        dimensions: Dimensions,
        window_state: WindowState,
        live_resizing: bool,
    },

    /// Called when a program-requested set_inner_size() has finished
    SetInnerSizeCompleted,

    /// Called when the window has been invalidated and needs to
    /// be repainted
    NeedRepaint,

    /// Called when the window gains/loses focus
    FocusChanged(bool),

    AdviseDeadKeyStatus(DeadKeyStatus),

    /// Called to handle a raw key event, prior to any dead key,
    /// keymap composition or other higher level treatment.
    /// If you handle this key event, you must call
    /// event.set_handled() to prevent additional processing.
    RawKeyEvent(RawKeyEvent),

    /// Called to handle a key event.
    KeyEvent(KeyEvent),

    MouseEvent(MouseEvent),
    MouseLeave,

    AppearanceChanged(Appearance),

    Notification(Box<dyn Any + Send + Sync>),

    // Called when the files are being dragged into the window
    DraggedFile(Vec<PathBuf>),

    // Called when the files are dropped into the window
    DroppedFile(Vec<PathBuf>),

    // Called when urls are dropped into the window
    DroppedUrl(Vec<Url>),

    // Called when text is dropped into the window
    DroppedString(String),

    /// Called by menubar dispatching stuff on some systems
    PerformKeyAssignment(config::keyassignment::KeyAssignment),

    AdviseModifiersLedStatus(Modifiers, KeyboardLedStatus),
}

impl WindowEvent {
    /// The event as a log line that holds no typed, composed or dropped
    /// text, path, URL or key assignment: keys keep only whether they
    /// went down, the others their variant.
    pub fn without_text(&self) -> String {
        match self {
            Self::KeyEvent(event) => format!("KeyEvent {{ key_is_down: {} }}", event.key_is_down),
            Self::RawKeyEvent(event) => {
                format!("RawKeyEvent {{ key_is_down: {} }}", event.key_is_down)
            }
            Self::AdviseDeadKeyStatus(DeadKeyStatus::Composing(_)) => {
                "AdviseDeadKeyStatus(Composing)".to_string()
            }
            Self::DraggedFile(paths) => format!("DraggedFile({} paths)", paths.len()),
            Self::DroppedFile(paths) => format!("DroppedFile({} paths)", paths.len()),
            Self::DroppedUrl(urls) => format!("DroppedUrl({} urls)", urls.len()),
            Self::DroppedString(_) => "DroppedString".to_string(),
            Self::PerformKeyAssignment(_) => "PerformKeyAssignment".to_string(),
            Self::AdviseDeadKeyStatus(DeadKeyStatus::None)
            | Self::CloseRequested
            | Self::Destroyed
            | Self::SurfaceAvailable { .. }
            | Self::SurfaceLost
            | Self::Resized { .. }
            | Self::SetInnerSizeCompleted
            | Self::NeedRepaint
            | Self::FocusChanged(_)
            | Self::MouseEvent(_)
            | Self::MouseLeave
            | Self::AppearanceChanged(_)
            | Self::Notification(_)
            | Self::AdviseModifiersLedStatus(..) => format!("{self:?}"),
        }
    }
}

pub struct WindowEventSender {
    handler: Box<dyn FnMut(WindowEvent, &Window)>,
    window: Option<Window>,
}

impl WindowEventSender {
    pub fn new<F: 'static + FnMut(WindowEvent, &Window)>(handler: F) -> Self {
        Self {
            handler: Box::new(handler),
            window: None,
        }
    }

    pub(crate) fn assign_window(&mut self, window: Window) {
        self.window.replace(window);
    }

    pub fn dispatch(&mut self, event: WindowEvent) {
        if let Some(window) = self.window.as_ref() {
            log::trace!("{:?}", event);
            (self.handler)(event, window);
        }
    }
}

#[derive(Debug, Error)]
#[error("Graphics drivers lost context")]
pub struct GraphicsDriversLostContext {}

/// Shared ownership of the native presentation surface behind a window's
/// raw handle.  GPU state that targets the handle keeps a clone, so the
/// backend releases the native surface only after every GPU reference to
/// it is gone.
#[derive(Clone)]
pub struct SurfaceLease {
    _holder: std::sync::Arc<dyn Send + Sync>,
}

impl SurfaceLease {
    pub fn new(holder: std::sync::Arc<dyn Send + Sync>) -> Self {
        Self { _holder: holder }
    }
}

impl std::fmt::Debug for SurfaceLease {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("SurfaceLease")
    }
}

#[async_trait(?Send)]
pub trait WindowOps {
    /// Show a hidden window
    fn show(&self);

    fn notify<T: Any + Send + Sync>(&self, t: T)
    where
        Self: Sized;

    /// Setup opengl for rendering
    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>>;
    /// Advise the window that a frame is finished
    fn finish_frame(&self, frame: glium::Frame) -> anyhow::Result<()> {
        frame.finish()?;
        Ok(())
    }

    /// Advise the window that a frame was presented on its surface.
    fn frame_presented(&self) {}

    /// The lease behind `window_handle()` when the backend leases its
    /// presentation surfaces (Android).  GPU state built on the handle
    /// holds it for as long as it references the surface.
    fn surface_lease(&self) -> Option<SurfaceLease> {
        None
    }

    /// Hide a visible window
    fn hide(&self);

    /// Schedule the window to be closed
    fn close(&self);

    /// Change the cursor, `None` hides the cursor
    fn set_cursor(&self, cursor: Option<CursorIcon>);

    /// Invalidate the window so that the entire client area will
    /// be repainted shortly
    fn invalidate(&self);

    /// Change the titlebar text for the window
    fn set_title(&self, title: &str);

    /// Resize the inner or client area of the window
    fn set_inner_size(&self, width: usize, height: usize);

    /// Use for windows snap layouts
    fn set_maximize_button_position(&self, _rect: ScreenRect) {}

    /// Requests the windowing system to start a window drag.
    ///
    /// This is only implemented on backends that handle
    /// window movement on the server side (Wayland).
    fn request_drag_move(&self) {}

    /// Signal to the windowing system that the mouse is over
    /// a window dragging area.
    ///
    /// This is only implemented on backends that need to
    /// know if the mouse is in a drag area to handle the
    /// click before forwarding the event (Windows).
    fn set_window_drag_position(&self, _coords: ScreenPoint) {}

    /// Changes the location of the window on the screen.
    /// The coordinates are of the top left pixel of the
    /// client area.
    ///
    /// This is only implemented on backends that allow
    /// windows to move themselves (not Wayland).
    fn set_window_position(&self, _coords: ScreenPoint) {}

    /// inform the windowing system of the current textual
    /// cursor input location.  This is used primarily for
    /// the platform specific input method editor
    fn set_text_cursor_position(&self, _cursor: Rect) {}

    /// Initiate textual transfer from the clipboard
    fn get_clipboard(&self, clipboard: Clipboard) -> Future<String>;

    /// Set some text in the clipboard
    fn set_clipboard(&self, clipboard: Clipboard, text: String);

    /// Set window level. Depending on the environment and user preferences
    fn set_window_level(&self, _level: WindowLevel) {}

    /// Set the icon for the window.
    /// Depending on the system this may be shown in its titlebar
    /// and/or in the task manager/task switcher
    fn set_icon(&self, _image: Image) {}

    fn maximize(&self) {}
    fn restore(&self) {}
    fn focus(&self) {}

    fn toggle_fullscreen(&self) {}

    fn config_did_change(&self, _config: &config::ConfigHandle) {}

    /// Configure the Window so that the desktop environment
    /// will constrain resizes so that they are multiples of
    /// the x and y values specified.
    /// This may not be supported or respected by the desktop
    /// environment.
    fn set_resize_increments(&self, _incr: ResizeIncrement) {}

    fn get_os_parameters(
        &self,
        _config: &ConfigHandle,
        _window_state: WindowState,
    ) -> anyhow::Result<Option<os::parameters::Parameters>> {
        Ok(None)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RequestedWindowGeometry {
    pub width: Dimension,
    pub height: Dimension,
    pub x: Option<Dimension>,
    pub y: Option<Dimension>,
    /// Specifies basis for evaluating x/y coords.
    /// Also applies to width/height when computing % based dimensions
    pub origin: GeometryOrigin,
}

#[derive(Debug, Clone)]
pub struct ResolvedGeometry {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ResizeIncrement {
    pub x: u16,
    pub y: u16,
    pub base_width: u16,
    pub base_height: u16,
}

impl ResizeIncrement {
    /// Use this as a readable shorthand for disabling the feature
    pub fn disabled() -> Self {
        Self {
            x: 1,
            y: 1,
            base_width: 0,
            base_height: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(key: KeyCode) -> RawKeyEvent {
        RawKeyEvent {
            key,
            modifiers: Modifiers::NONE,
            leds: KeyboardLedStatus::empty(),
            phys_code: None,
            raw_code: 0x34,
            #[cfg(windows)]
            scan_code: 0,
            repeat_count: 1,
            key_is_down: true,
            handled: Handled::new(),
        }
    }

    #[test]
    fn events_carrying_text_log_without_it() {
        let key = KeyEvent {
            key: KeyCode::Char('S'),
            modifiers: Modifiers::SHIFT,
            leds: KeyboardLedStatus::empty(),
            repeat_count: 1,
            key_is_down: true,
            raw: Some(raw(KeyCode::Char('S'))),
            #[cfg(windows)]
            win32_uni_char: None,
        };
        let logged = [
            WindowEvent::KeyEvent(key),
            WindowEvent::KeyEvent(KeyEvent {
                key: KeyCode::Composed("e\u{301}".to_string()),
                modifiers: Modifiers::NONE,
                leds: KeyboardLedStatus::empty(),
                repeat_count: 1,
                key_is_down: false,
                raw: None,
                #[cfg(windows)]
                win32_uni_char: None,
            }),
            WindowEvent::RawKeyEvent(raw(KeyCode::Char('x'))),
            WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::Composing("nihao".to_string())),
            WindowEvent::DroppedString("hunter2\n".to_string()),
            WindowEvent::DroppedFile(vec![PathBuf::from("/home/secret.txt")]),
            WindowEvent::DraggedFile(vec![PathBuf::from("a"), PathBuf::from("b")]),
            WindowEvent::DroppedUrl(vec![Url::parse("https://example.com/?token=x").unwrap()]),
            WindowEvent::PerformKeyAssignment(config::keyassignment::KeyAssignment::SendString(
                "hunter2".to_string(),
            )),
        ]
        .iter()
        .map(WindowEvent::without_text)
        .collect::<Vec<_>>();
        assert_eq!(
            logged,
            [
                "KeyEvent { key_is_down: true }",
                "KeyEvent { key_is_down: false }",
                "RawKeyEvent { key_is_down: true }",
                "AdviseDeadKeyStatus(Composing)",
                "DroppedString",
                "DroppedFile(1 paths)",
                "DraggedFile(2 paths)",
                "DroppedUrl(1 urls)",
                "PerformKeyAssignment",
            ]
        );
    }

    #[test]
    fn events_without_text_log_their_debug_form() {
        let logged = [
            WindowEvent::FocusChanged(true),
            WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None),
            WindowEvent::SurfaceLost,
        ]
        .iter()
        .map(WindowEvent::without_text)
        .collect::<Vec<_>>();
        assert_eq!(
            logged,
            [
                "FocusChanged(true)",
                "AdviseDeadKeyStatus(None)",
                "SurfaceLost"
            ]
        );
    }
}
