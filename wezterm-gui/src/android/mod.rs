//! Android GUI engine entry.

use crate::frontend;
use mux::Mux;
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
/// `ready` runs exactly once before the loop starts, with the bootstrap
/// result.  From a successful `ready` on, the promise scheduler accepts
/// work for this thread.
pub fn run(options: GuiOptions, ready: impl FnOnce(anyhow::Result<()>)) -> anyhow::Result<()> {
    let gui = match bootstrap(options) {
        Ok(gui) => gui,
        Err(err) => {
            ready(Err(anyhow::anyhow!("{err:#}")));
            return Err(err);
        }
    };
    ready(Ok(()));
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
        promise::spawn::spawn(async {
            if let Err(err) = diagnostic::run().await {
                log::error!("diagnostic applet ended: {err:#}");
            }
        })
        .detach();
        #[cfg(not(debug_assertions))]
        log::warn!("the diagnostic applet exists only in debug builds");
    }
    Ok(gui)
}
