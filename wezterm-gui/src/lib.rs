//! WezTerm GUI as a library.
//!
//! The desktop binary (`main.rs`) and the Android JNI crate both link this
//! library.  Rendering, font, input and mux-frontend modules live here once;
//! only the entry points differ per platform.

use crate::utilsprites::RenderMetrics;
use ::window::{color, glium, Dimensions};
use config::ConfigHandle;
use mux::activity::Activity;
use mux::Mux;
use std::rc::Rc;
use wezterm_font::FontConfiguration;

mod colorease;
mod commands;
mod customglyph;
#[cfg(not(target_os = "android"))]
pub mod desktop;
mod download;
mod frontend;
pub mod glyphcache;
mod inputmap;
mod overlay;
mod quad;
mod renderstate;
mod resize_increment_calculator;
mod scripting;
mod scrollbar;
mod selection;
mod shapecache;
mod spawn;
mod stats;
mod tabbar;
mod termwindow;
mod unicode_names;
mod uniforms;
mod update;
pub mod utilsprites;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

pub(crate) fn cell_pixel_dims(config: &ConfigHandle, dpi: f64) -> anyhow::Result<(usize, usize)> {
    let fontconfig = Rc::new(FontConfiguration::new(Some(config.clone()), dpi as usize)?);
    let render_metrics = RenderMetrics::new(&fontconfig)?;
    Ok((
        render_metrics.cell_size.width as usize,
        render_metrics.cell_size.height as usize,
    ))
}

pub use selection::SelectionMode;
pub use termwindow::{set_window_class, set_window_position, TermWindow, ICON_DATA};
