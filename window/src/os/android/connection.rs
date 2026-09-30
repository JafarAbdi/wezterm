//! GUI-thread owner of the logical windows and of the one surface slot the
//! Activity's `SurfaceView` occupies.

#![forbid(unsafe_code)]

use super::monitor::surface_monitor;
use super::native_window::NativeWindowLease;
use super::window::{Window, WindowInner};
use super::AndroidSurfaceEvent;
use crate::connection::ConnectionOps;
use crate::spawn::SPAWN_QUEUE;
use crate::surface::{SurfaceEffect, SurfaceGeometry, SurfaceState};
use crate::{Dimensions, RequestedWindowGeometry, WindowEvent, WindowEventSender, WindowState};
use config::ConfigHandle;
use filedescriptor::{poll, pollfd, POLLIN};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wezterm_font::FontConfiguration;

static DISPLAY_DPI: AtomicUsize = AtomicUsize::new(0);

/// Publish the density of the display that hosts terminals.  Must precede
/// `Connection::init`; it becomes `default_dpi()`.
pub fn set_display_dpi(dpi: usize) {
    DISPLAY_DPI.store(dpi, Ordering::SeqCst);
}

/// The GUI engine has no promise scheduler yet, so nothing can run on the
/// GUI thread.
#[derive(Debug, thiserror::Error)]
#[error("the GUI engine is not running; surface events cannot be delivered")]
pub struct NotRunning;

/// Queue a surface callback for the GUI thread.  Safe from any thread; the
/// event is applied in order with every other GUI-thread task.
pub fn post_surface_event(event: AndroidSurfaceEvent) -> Result<(), NotRunning> {
    if !promise::spawn::is_scheduler_configured() {
        return Err(NotRunning);
    }
    promise::spawn::spawn_into_main_thread(async move {
        match Connection::get() {
            Some(conn) => conn.apply_surface_event(event),
            None => log::error!("surface event {event:?} arrived without a Connection"),
        }
    })
    .detach();
    Ok(())
}

pub struct Connection {
    dpi: usize,
    next_window_id: Cell<usize>,
    windows: RefCell<HashMap<usize, Rc<RefCell<WindowInner>>>>,
    surface: RefCell<SurfaceState<Arc<NativeWindowLease>>>,
    /// The logical window that presents on the surface slot; the first
    /// window binds.
    bound: Cell<Option<usize>>,
    terminate: Cell<bool>,
}

impl Connection {
    pub(crate) fn create_new() -> anyhow::Result<Self> {
        let dpi = DISPLAY_DPI.load(Ordering::SeqCst);
        anyhow::ensure!(dpi > 0, "set_display_dpi must precede Connection::init");
        // Create the spawn queue (and its wake pipe) on the GUI thread.
        SPAWN_QUEUE.run();
        Ok(Self {
            dpi,
            next_window_id: Cell::new(1),
            windows: RefCell::new(HashMap::new()),
            surface: RefCell::new(SurfaceState::default()),
            bound: Cell::new(None),
            terminate: Cell::new(false),
        })
    }

    pub async fn new_window<F>(
        &self,
        _class_name: &str,
        _name: &str,
        _geometry: RequestedWindowGeometry,
        _config: Option<&ConfigHandle>,
        _font_config: Rc<FontConfiguration>,
        event_handler: F,
    ) -> anyhow::Result<Window>
    where
        F: 'static + FnMut(WindowEvent, &Window),
    {
        let id = self.next_window_id.replace(self.next_window_id.get() + 1);
        let window = Window::new(id);
        let mut events = WindowEventSender::new(event_handler);
        events.assign_window(window);
        self.windows
            .borrow_mut()
            .insert(id, Rc::new(RefCell::new(WindowInner::new(events))));
        if self.bound.get().is_none() {
            self.bound.set(Some(id));
            surface_monitor().update(|s| s.bound_window = Some(id));
            // The caller finishes wiring its handler after this returns;
            // deliver the slot's current state on the next GUI-thread turn.
            promise::spawn::spawn(async move {
                if let Some(conn) = Connection::get() {
                    conn.present_current_surface(id);
                }
            })
            .detach();
        }
        Ok(window)
    }

    pub(super) fn window_by_id(&self, id: usize) -> Option<Rc<RefCell<WindowInner>>> {
        self.windows.borrow().get(&id).map(Rc::clone)
    }

    /// Run `f` against a window on the GUI thread.  Callable from any thread.
    pub(super) fn with_window_inner<F>(window_id: usize, f: F)
    where
        F: FnOnce(&mut WindowInner) + Send + 'static,
    {
        promise::spawn::spawn_into_main_thread(async move {
            if let Some(inner) = Connection::get().and_then(|conn| conn.window_by_id(window_id)) {
                f(&mut inner.borrow_mut());
            }
        })
        .detach();
    }

    pub(super) fn close_window(&self, id: usize) {
        let Some(inner) = self.windows.borrow_mut().remove(&id) else {
            return;
        };
        if self.bound.get() == Some(id) {
            self.bound.set(None);
            surface_monitor().update(|s| s.bound_window = None);
        }
        inner.borrow_mut().events.dispatch(WindowEvent::Destroyed);
    }

    /// The lease behind `window`'s raw handle, while it is bound and a
    /// surface exists.
    pub(super) fn surface_lease_for(&self, id: usize) -> Option<Arc<NativeWindowLease>> {
        if self.bound.get() != Some(id) {
            return None;
        }
        self.surface.borrow().lease().cloned()
    }

    fn bound_window(&self) -> Option<Rc<RefCell<WindowInner>>> {
        self.bound.get().and_then(|id| self.window_by_id(id))
    }

    fn dispatch_bound(&self, event: WindowEvent) {
        if let Some(inner) = self.bound_window() {
            inner.borrow_mut().events.dispatch(event);
        }
    }

    fn invalidate_bound(&self) {
        if let Some(inner) = self.bound_window() {
            inner.borrow_mut().invalidated = true;
        }
    }

    fn dimensions(&self, geometry: SurfaceGeometry) -> Dimensions {
        Dimensions {
            pixel_width: geometry.width() as usize,
            pixel_height: geometry.height() as usize,
            dpi: self.dpi,
        }
    }

    fn present_current_surface(&self, id: usize) {
        if self.bound.get() != Some(id) {
            return;
        }
        let geometry = self.surface.borrow().geometry();
        if let Some(geometry) = geometry {
            self.dispatch_available(geometry);
        }
    }

    fn dispatch_available(&self, geometry: SurfaceGeometry) {
        let dimensions = self.dimensions(geometry);
        self.dispatch_bound(WindowEvent::SurfaceAvailable { dimensions });
        self.dispatch_bound(WindowEvent::Resized {
            dimensions,
            window_state: WindowState::default(),
            live_resizing: false,
        });
        self.dispatch_bound(WindowEvent::FocusChanged(true));
        self.invalidate_bound();
    }

    fn apply_surface_event(&self, event: AndroidSurfaceEvent) {
        log::info!("surface event {event:?}");
        let (next, effects) = self.surface.take().apply(event);
        let generation = next.generation().map(|g| g.get());
        let geometry = next.geometry();
        let state_name = match &next {
            SurfaceState::Absent { .. } => "absent",
            SurfaceState::Unsized { .. } => "unsized",
            SurfaceState::Present { .. } => "present",
        };
        *self.surface.borrow_mut() = next;
        surface_monitor().update(|s| {
            if s.generation != generation {
                s.frames_presented = 0;
            }
            s.generation = generation;
            s.state = state_name;
            s.width = geometry.map_or(0, |g| g.width());
            s.height = geometry.map_or(0, |g| g.height());
        });

        for effect in effects {
            match effect {
                SurfaceEffect::Available(geometry) => self.dispatch_available(geometry),
                SurfaceEffect::Resized(geometry) => {
                    let dimensions = self.dimensions(geometry);
                    self.dispatch_bound(WindowEvent::Resized {
                        dimensions,
                        window_state: WindowState::default(),
                        live_resizing: false,
                    });
                    self.invalidate_bound();
                }
                SurfaceEffect::Lost => {
                    self.dispatch_bound(WindowEvent::FocusChanged(false));
                    self.dispatch_bound(WindowEvent::SurfaceLost);
                }
                SurfaceEffect::Retire { lease, ack } => {
                    log::info!(
                        "retiring surface generation {} with {} lease holder(s) remaining",
                        lease.generation().get(),
                        Arc::strong_count(&lease)
                    );
                    if let Some(ack) = ack {
                        lease.arm_retire(ack);
                    }
                    drop(lease);
                }
                SurfaceEffect::Ack(ack) => ack.send(),
                SurfaceEffect::Stale(generation) => {
                    log::warn!(
                        "ignoring surface callback for stale generation {}",
                        generation.get()
                    );
                    surface_monitor().update(|s| s.stale_events += 1);
                }
            }
        }
    }

    /// Paint the bound window once if it was invalidated and can present.
    fn paint_invalidated(&self) {
        let Some(inner) = self.bound_window() else {
            return;
        };
        if !self.surface.borrow().is_present() {
            return;
        }
        let needs_paint = std::mem::replace(&mut inner.borrow_mut().invalidated, false);
        if needs_paint {
            inner.borrow_mut().events.dispatch(WindowEvent::NeedRepaint);
        }
    }
}

impl ConnectionOps for Connection {
    fn name(&self) -> String {
        "Android".to_string()
    }

    fn default_dpi(&self) -> f64 {
        self.dpi as f64
    }

    fn terminate_message_loop(&self) {
        self.terminate.set(true);
        promise::spawn::spawn_into_main_thread(async {}).detach();
    }

    /// Sleep until the spawn queue is signalled, drain it, then paint.
    /// Every wake-up is a queued task: surface events, mux notifications,
    /// timers and invalidations all arrive through the queue.
    fn run_message_loop(&self) -> anyhow::Result<()> {
        let fd = SPAWN_QUEUE.raw_fd();
        while !self.terminate.get() {
            let mut pfd = [pollfd {
                fd,
                events: POLLIN,
                revents: 0,
            }];
            match poll(&mut pfd, None) {
                Ok(_) => {}
                Err(filedescriptor::Error::Poll(ref err))
                    if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err.into()),
            }
            while SPAWN_QUEUE.run() {}
            self.paint_invalidated();
        }
        Ok(())
    }
}
