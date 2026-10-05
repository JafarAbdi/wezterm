//! Narrow Android adapters to the native GUI frontend and private TermWindow.

use crate::frontend;
use crate::termwindow::TermWindowNotif;
use ::window::{Connection, ConnectionOps, WindowOps};
use anyhow::Context;
use mux::pane::{Pane, PaneId};
use mux::tab::TabId;
use mux::window::WindowId as MuxWindowId;
use mux::{Mux, MuxNotification};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

/// Handle to the native frontend; its scheduler and terminal windows stay private.
pub struct Frontend(Rc<frontend::GuiFrontEnd>);

impl Frontend {
    /// Initialize after the caller has registered its mux client identity.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self(frontend::try_new()?))
    }

    /// Run the existing GUI-thread scheduler and platform message loop.
    pub fn run_forever(&self) -> anyhow::Result<()> {
        self.0.run_forever()
    }
}

/// Release the frontend after the caller shuts down its mux.
pub fn shutdown_frontend() {
    frontend::shutdown();
}

/// GUI thread: the window bound to the surface and its mux window.
fn bound_window() -> anyhow::Result<(::window::Window, MuxWindowId)> {
    let bound = Connection::get()
        .and_then(|conn| conn.bound_window())
        .context("no window is bound")?;
    let gui_window = frontend::front_end()
        .gui_windows()
        .into_iter()
        .find(|gui_window| gui_window.window == bound)
        .context("the bound window has no terminal window")?;
    Ok((bound, gui_window.mux_window_id))
}

/// GUI thread: the window bound to the surface and the pane its keyboard
/// input goes to.
pub fn bound_pane() -> anyhow::Result<(::window::Window, Arc<dyn Pane>)> {
    let (bound, mux_window) = bound_window()?;
    let pane = Mux::get()
        .get_active_tab_for_window(mux_window)
        .and_then(|tab| tab.get_active_pane())
        .context("the bound window has no active pane")?;
    Ok((bound, pane))
}

/// A mux change after which a window's keyboard input reaches another
/// pane, as the notification announcing it names it: the tab whose active
/// pane changed, or the window whose active tab changed or that was
/// removed.  A change of size announces neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetChange {
    Tab(TabId),
    Window(MuxWindowId),
}

impl TargetChange {
    pub fn of(notification: &MuxNotification) -> Option<Self> {
        use MuxNotification as N;
        match *notification {
            N::ActivePaneChanged(tab) => Some(Self::Tab(tab)),
            N::ActiveTabChanged(window) | N::WindowRemoved(window) => Some(Self::Window(window)),
            _ => None,
        }
    }
}

/// GUI thread: the pane the bound window's keyboard input reaches, and
/// whether any of `changes`, recorded since the last call, moved it, also
/// away and back.  A tab's change counts while that tab is the bound
/// window's active one; the window announces every change of which tab
/// that is.
pub fn input_target(changes: &[TargetChange]) -> (Option<PaneId>, bool) {
    let Ok((_, bound)) = bound_window() else {
        return (None, false);
    };
    let tab = Mux::get().get_active_tab_for_window(bound);
    let shown = tab.as_ref().map(|tab| tab.tab_id());
    let moved = changes.iter().any(|change| match *change {
        TargetChange::Tab(tab) => Some(tab) == shown,
        TargetChange::Window(window) => window == bound,
    });
    let pane = tab
        .and_then(|tab| tab.get_active_pane())
        .map(|pane| pane.pane_id());
    (pane, moved)
}

thread_local! {
    /// Set while `fit_bound_window` applies dimensions: the `TabResized`
    /// notifications that raises are its own.
    static FITTING: Cell<bool> = const { Cell::new(false) };
}

/// Whether the calling thread is inside `fit_bound_window`'s resize.
pub fn fitting() -> bool {
    FITTING.get()
}

/// GUI thread: when any tab of the bound window holds other rows or
/// columns than the window's surface gives (a resync applied the mux
/// server's sizes, or the window shows again after another client resized
/// it), the window applies its dimensions again, as a resize of its
/// surface does: the tabs, and the server's panes, take the surface's
/// size.  Only while the window shows: decided when it would resize, so a
/// bound window that shows nothing leaves the size another client set.
pub fn fit_bound_window() {
    let Some(bound) = Connection::get().and_then(|conn| conn.bound_window()) else {
        return;
    };
    bound.notify(TermWindowNotif::Apply(Box::new(|tw| {
        let shown = tw
            .window
            .is_some_and(|window| Connection::get().is_some_and(|conn| conn.presents(window)));
        if !shown {
            log::debug!("the bound window shows nothing; its tabs keep their size");
            return;
        }
        let want = tw.current_cell_dimensions();
        let stale = Mux::get()
            .get_window(tw.mux_window_id)
            .is_some_and(|window| {
                window.iter_tabs().any(|tab| {
                    let size = tab.get_size();
                    (size.rows, size.cols) != (want.rows, want.cols)
                })
            });
        let Some(window) = tw.window.clone().filter(|_| stale) else {
            return;
        };
        let dimensions = tw.dimensions;
        log::debug!("fitting the bound window's tabs to {want:?}");
        FITTING.set(true);
        tw.apply_dimensions(&dimensions, None, &window);
        FITTING.set(false);
    })));
}

/// GUI thread: apply absolute native point sizes to every logical window. The
/// typed config default also supplies future windows; no input is generated.
/// False if native metrics or a window's atlas could not accept the choice.
pub fn set_font_sizes(size: config::NativeFontSizes) -> bool {
    let Some(conn) = Connection::get() else {
        return false;
    };
    if crate::mobile_font::prepare(&config::configuration(), conn.default_dpi() as usize, size)
        .is_err()
        || !font_size_windows(size, true)
    {
        return false;
    }
    let prior_size = config::NativeFontSizes::from(&*config::configuration());
    if !font_size_windows(size, false) {
        // A resource failure is not an applied choice. Restore the last choice
        // rather than saving a partially applied size on the platform.
        font_size_windows(prior_size, false);
        return false;
    }
    config::set_native_font_defaults(size);
    let configuration = config::configuration().with_native_font_sizes(size);
    config::use_this_configuration((*configuration).clone());
    true
}

fn font_size_windows(size: config::NativeFontSizes, check_only: bool) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    for window in frontend::front_end().gui_windows() {
        let accepted = Arc::new(AtomicBool::new(false));
        let result = Arc::clone(&accepted);
        let delivered = window
            .window
            .notify_now(TermWindowNotif::Apply(Box::new(move |tw| {
                let applied = if check_only {
                    tw.check_native_font_sizes(size)
                } else {
                    tw.set_native_font_sizes(size)
                };
                result.store(applied, Ordering::Relaxed);
            })));
        if !delivered || !accepted.load(Ordering::Relaxed) {
            return false;
        }
    }
    true
}

pub use crate::inputmap::is_paste_key;
