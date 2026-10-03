//! Android GUI engine entry.

use crate::frontend;
use crate::inputmap::InputMap;
use crate::termwindow::TermWindowNotif;
use ::window::{Connection, ConnectionOps, KeyEvent, WindowOps};
use anyhow::Context;
use config::keyassignment::KeyAssignment;
use mux::pane::{Pane, PaneId};
use mux::tab::TabId;
use mux::window::WindowId as MuxWindowId;
use mux::{Mux, MuxNotification};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

#[cfg(debug_assertions)]
pub mod diagnostic;

/// What the engine runs.
#[derive(Debug, Clone, Copy)]
pub struct GuiOptions {
    /// Density of the display that hosts terminals; becomes `default_dpi()`.
    pub dpi: usize,
    /// Open the diagnostic applet window on start.  Debug builds only; a
    /// release build logs and ignores it.
    pub diagnostic_applet: bool,
}

/// Run the GUI on the calling thread until its message loop terminates.
///
/// `running` runs once, after a successful bootstrap and before the loop
/// starts.  From then on the promise scheduler accepts work for this
/// thread.
pub fn run(options: GuiOptions, running: impl FnOnce()) -> anyhow::Result<()> {
    let gui = bootstrap(options)?;
    running();
    let result = gui.run_forever();
    Mux::shutdown();
    frontend::shutdown();
    result
}

fn bootstrap(options: GuiOptions) -> anyhow::Result<Rc<frontend::GuiFrontEnd>> {
    config::designate_this_as_the_main_thread();
    ::window::os::android::set_display_dpi(options.dpi);

    let mux = Arc::new(Mux::new(None));
    Mux::set_mux(&mux);
    let client_id = Arc::new(mux::client::ClientId::new());
    mux.register_client(client_id.clone());
    mux.replace_identity(Some(client_id));
    mux.set_active_workspace(mux::DEFAULT_WORKSPACE);

    let gui = frontend::try_new()?;

    if options.diagnostic_applet {
        #[cfg(debug_assertions)]
        diagnostic::open_window();
        #[cfg(not(debug_assertions))]
        log::warn!("the diagnostic applet exists only in debug builds");
    }
    Ok(gui)
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

/// A mux change after which a window's keyboard input may reach another
/// pane, as the notification announcing it names it.  Every write of a
/// tab's active pane is announced in the same call: `PaneFocused` by a
/// focus change, `TabResized` by a resync (`Tab::sync_with_pane_tree` sets
/// the active pane without `PaneFocused`, then resizes the tab), and a
/// tab switch, insertion or removal by its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetChange {
    Pane(PaneId),
    Tab(TabId),
    Window(MuxWindowId),
}

impl TargetChange {
    pub fn of(notification: &MuxNotification) -> Option<Self> {
        use MuxNotification as N;
        match *notification {
            N::PaneFocused(pane) => Some(Self::Pane(pane)),
            N::TabResized(tab) => Some(Self::Tab(tab)),
            N::WindowInvalidated(window)
            | N::WindowRemoved(window)
            | N::TabAddedToWindow {
                window_id: window, ..
            } => Some(Self::Window(window)),
            _ => None,
        }
    }

    /// The mux window the change happened in; `None` once it no longer
    /// resolves.
    fn window(self, mux: &Mux) -> Option<MuxWindowId> {
        match self {
            Self::Pane(pane) => mux.resolve_pane_id(pane).map(|(_, window, _)| window),
            Self::Tab(tab) => mux.window_containing_tab(tab),
            Self::Window(window) => Some(window),
        }
    }
}

/// GUI thread: the pane the bound window's keyboard input reaches, and
/// whether any of `changes`, recorded since the last call, may have moved
/// it, also away and back.  A change that no longer resolves to a window
/// counts.
pub fn input_target(changes: &[TargetChange]) -> (Option<PaneId>, bool) {
    let Ok((_, bound)) = bound_window() else {
        return (None, false);
    };
    let mux = Mux::get();
    let moved = changes
        .iter()
        .any(|change| change.window(&mux).is_none_or(|window| window == bound));
    let pane = mux
        .get_active_tab_for_window(bound)
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
/// columns than the window's surface gives (a resync applied the laptop's
/// sizes, or the window shows again after the laptop resized it), the
/// window applies its dimensions again, as a resize of its surface does:
/// the tabs, and the laptop's panes, take the surface's size.  Only while
/// the window shows: decided when it would resize, so a phone that shows
/// nothing leaves the laptop's size as the laptop set it.
pub fn fit_bound_window() {
    let Some(bound) = Connection::get().and_then(|conn| conn.bound_window()) else {
        return;
    };
    bound.notify(TermWindowNotif::Apply(Box::new(|tw| {
        let shown = tw
            .window
            .is_some_and(|window| Connection::get().is_some_and(|conn| conn.presents(window)));
        if !shown {
            log::debug!("the bound window shows nothing; its tabs keep the laptop's size");
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

/// Whether the key table makes `key` a paste.  `TermWindow` would read
/// the clipboard for it asynchronously, after the input queued behind the
/// key; the platform pastes the clipboard it holds in order instead.
/// Callable from any thread.
pub fn is_paste_key(key: &KeyEvent) -> bool {
    InputMap::new(&config::configuration())
        .lookup_key(&key.key, key.modifiers, None)
        .is_some_and(|entry| matches!(entry.action, KeyAssignment::PasteFrom(_)))
}
