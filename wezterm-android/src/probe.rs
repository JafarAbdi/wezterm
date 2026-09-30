//! Diagnostics that exercise the linked native closure.
//!
//! Each probe calls into a real subsystem (OpenSSL, zstd, libgit2, Lua,
//! FreeType, HarfBuzz, the GUI glyph cache) so that the linker cannot strip
//! the closure down to a stub and a green load cannot be faked.

#![forbid(unsafe_code)]

use anyhow::Context;
use config::{AndroidPaths, ConfigHandle};
use serde::Serialize;
use std::rc::Rc;
use termwiz::cell::CellAttributes;
use termwiz::surface::{Line, SEQ_ZERO};
use wezterm_font::FontConfiguration;
use wezterm_font::shaper::PresentationWidth;
use wezterm_gui::glyphcache::GlyphCache;
use wezterm_gui::utilsprites::RenderMetrics;

/// Versions and offline self-checks of native libraries that the
/// SSH/mux/config/render stack depends on.
#[derive(Debug, Clone, Serialize)]
pub struct NativeVersions {
    /// `OpenSSL_version(OPENSSL_VERSION)`; a TLS `SSL_CTX` was also built.
    pub openssl: String,
    /// `ZSTD_versionString()`; a compress/decompress round trip succeeded.
    pub zstd: String,
    /// libgit2 major.minor.patch.
    pub libgit2: String,
    /// Lua `_VERSION` from a WezTerm-configured interpreter.
    pub lua: String,
    /// libssh `ssh_new()` succeeded (no connection is made).
    pub libssh_session: bool,
    /// libssh2 `libssh2_session_init()` succeeded (no connection is made).
    pub libssh2_session: bool,
    /// Mux codec version whose `Ping` PDU round-tripped through encode/decode.
    pub codec_roundtrip_version: usize,
    /// GPU adapters wgpu can enumerate without a surface.
    pub gpu_adapters: Vec<GpuAdapter>,
}

/// One wgpu adapter as seen on this device.
#[derive(Debug, Clone, Serialize)]
pub struct GpuAdapter {
    /// Driver-reported adapter name.
    pub name: String,
    /// `Vulkan`, `Gl`, ...
    pub backend: String,
    /// `IntegratedGpu`, `Cpu`, ...
    pub device_type: String,
    /// Driver name and version info.
    pub driver: String,
}

/// Fonts the WezTerm font engine resolved without any system font service.
#[derive(Debug, Clone, Serialize)]
pub struct FontReport {
    /// Configured locator (expected `ConfigDirsOnly` on Android).
    pub locator: String,
    /// DPI used for metrics.
    pub dpi: u32,
    /// Fallback chain of the default font, as `wezterm ls-fonts` prints it.
    pub default_font: Vec<String>,
    /// Fallback chain of the title font.
    pub title_font: Vec<String>,
    /// Fonts discovered in `font_dirs` plus built-ins.
    pub font_dir_count: usize,
}

/// Result of shaping and rasterizing sample text through the GUI glyph cache.
#[derive(Debug, Clone, Serialize)]
pub struct ShapingProbe {
    /// The text that was shaped.
    pub text: String,
    /// Cell size in pixels at the requested DPI.
    pub cell_size: (isize, isize),
    /// Glyphs returned by the shaper.
    pub glyph_count: usize,
    /// Glyphs that produced a texture in the in-memory atlas.
    pub rasterized_count: usize,
    /// Distinct fonts used across the shaped glyphs.
    pub fonts_used: Vec<String>,
}

/// Query native library versions.  Lua runs through the same context
/// builder the configuration system uses.
pub(crate) fn native_versions(dirs: &AndroidPaths) -> anyhow::Result<NativeVersions> {
    let lua = config::lua::make_lua_context(&dirs.config.join("wezterm.lua"))
        .context("make_lua_context")?;
    let lua_version: String = lua
        .load("return _VERSION")
        .eval()
        .context("evaluate _VERSION")?;
    let git = git2::Version::get();
    let (major, minor, patch) = git.libgit2_version();

    openssl::ssl::SslContext::builder(openssl::ssl::SslMethod::tls()).context("SSL_CTX_new")?;
    let sample = b"wezterm android native closure probe".repeat(8);
    let compressed = zstd::bulk::compress(&sample, 3).context("ZSTD_compress")?;
    let restored = zstd::bulk::decompress(&compressed, sample.len()).context("ZSTD_decompress")?;
    anyhow::ensure!(restored == sample, "zstd round trip mismatch");

    let libssh_session = libssh_rs::Session::new().context("libssh ssh_new")?;
    drop(libssh_session);
    let libssh2_session = ssh2::Session::new().context("libssh2_session_init")?;
    drop(libssh2_session);

    let mut encoded = Vec::new();
    codec::Pdu::Ping(codec::Ping {})
        .encode(&mut encoded, 7)
        .context("codec encode")?;
    let decoded = codec::Pdu::decode(encoded.as_slice()).context("codec decode")?;
    anyhow::ensure!(
        decoded.serial == 7 && matches!(decoded.pdu, codec::Pdu::Ping(_)),
        "codec round trip mismatch: {decoded:?}"
    );

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let gpu_adapters = instance
        .enumerate_adapters(wgpu::Backends::all())
        .iter()
        .map(|adapter| {
            let info = adapter.get_info();
            GpuAdapter {
                name: info.name,
                backend: format!("{:?}", info.backend),
                device_type: format!("{:?}", info.device_type),
                driver: format!("{} {}", info.driver, info.driver_info),
            }
        })
        .collect();

    Ok(NativeVersions {
        openssl: openssl::version::version().to_string(),
        zstd: zstd::zstd_safe::version_string().to_string(),
        libgit2: format!("{major}.{minor}.{patch}"),
        lua: lua_version,
        libssh_session: true,
        libssh2_session: true,
        codec_roundtrip_version: codec::CODEC_VERSION,
        gpu_adapters,
    })
}

fn font_config(config: &ConfigHandle, dpi: u32) -> anyhow::Result<Rc<FontConfiguration>> {
    Ok(Rc::new(FontConfiguration::new(
        Some(config.clone()),
        dpi as usize,
    )?))
}

fn handle_names(font: &wezterm_font::LoadedFont) -> Vec<String> {
    font.clone_handles()
        .iter()
        .map(|parsed| {
            format!(
                "{} {}",
                parsed.lua_name(),
                parsed.handle.diagnostic_string()
            )
        })
        .collect()
}

/// Resolve the default and title fonts with the real font engine.
pub(crate) fn fonts(config: &ConfigHandle, dpi: u32) -> anyhow::Result<FontReport> {
    let fonts = font_config(config, dpi)?;
    let default_font = fonts.default_font().context("default_font")?;
    let title_font = fonts.title_font().context("title_font")?;
    Ok(FontReport {
        locator: format!("{:?}", config.font_locator),
        dpi,
        default_font: handle_names(&default_font),
        title_font: handle_names(&title_font),
        font_dir_count: fonts.list_fonts_in_font_dirs().len(),
    })
}

/// Shape `text` with HarfBuzz and rasterize every glyph with FreeType into
/// an in-memory atlas, exactly as `wezterm ls-fonts --rasterize-ascii` does.
pub(crate) fn shape_and_rasterize(
    config: &ConfigHandle,
    dpi: u32,
    text: &str,
) -> anyhow::Result<ShapingProbe> {
    let fonts = font_config(config, dpi)?;
    let metrics = RenderMetrics::new(&fonts).context("RenderMetrics")?;
    let mut glyph_cache = GlyphCache::new_in_memory(&fonts, 256).context("GlyphCache")?;

    let line = Line::from_text(
        text,
        &CellAttributes::default(),
        SEQ_ZERO,
        Some(&config.unicode_version()),
    );
    let mut glyph_count = 0;
    let mut rasterized_count = 0;
    let mut fonts_used = Vec::new();
    for cluster in line.cluster(None) {
        let style = fonts.match_style(config, &cluster.attrs);
        let font = fonts.resolve_font(style).context("resolve_font")?;
        let presentation_width = PresentationWidth::with_cluster(&cluster);
        let infos = font
            .blocking_shape(
                &cluster.text,
                Some(cluster.presentation),
                cluster.direction,
                None,
                Some(&presentation_width),
            )
            .context("blocking_shape")?;
        let handles = font.clone_handles();
        for info in &infos {
            glyph_count += 1;
            let glyph = glyph_cache
                .cached_glyph(info, style, false, &font, &metrics, info.num_cells)
                .context("cached_glyph")?;
            if glyph.texture.is_some() {
                rasterized_count += 1;
            }
            let name = handles[info.font_idx].lua_name();
            if !fonts_used.contains(&name) {
                fonts_used.push(name);
            }
        }
    }
    Ok(ShapingProbe {
        text: text.to_string(),
        cell_size: (metrics.cell_size.width, metrics.cell_size.height),
        glyph_count,
        rasterized_count,
        fonts_used,
    })
}
