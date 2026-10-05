//! Application-supplied native GUI font sizes, in points (72 per inch).

use std::sync::{Arc, RwLock};

/// Absolute sizes for each independently sized native GUI entity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeFontSizes {
    pub body: f64,
    pub title: f64,
    pub command_palette: f64,
    pub char_select: f64,
    pub pane_select: f64,
}

impl From<&crate::Config> for NativeFontSizes {
    fn from(config: &crate::Config) -> Self {
        Self {
            body: config.font_size,
            title: config.window_frame.font_size.unwrap_or(config.font_size),
            command_palette: config.command_palette_font_size,
            char_select: config.char_select_font_size,
            pane_select: config.pane_select_font_size,
        }
    }
}

static DEFAULTS: RwLock<Option<NativeFontSizes>> = RwLock::new(None);

/// Publish typed defaults before configuration initialization, and after a
/// successful native application. Reloads and overridden configs inherit these
/// through the ordinary default factories; explicit config fields still win.
pub fn set_native_font_defaults(sizes: NativeFontSizes) {
    *DEFAULTS.write().unwrap() = Some(sizes);
}

pub(crate) fn native_font_defaults() -> Option<NativeFontSizes> {
    *DEFAULTS.read().unwrap()
}

impl crate::ConfigHandle {
    /// Copy absolute sizes without changing styles, colors or generation.
    pub fn with_native_font_sizes(&self, sizes: NativeFontSizes) -> Self {
        let mut config = (**self).clone();
        config.font_size = sizes.body;
        config.window_frame.font_size = Some(sizes.title);
        config.command_palette_font_size = sizes.command_palette;
        config.char_select_font_size = sizes.char_select;
        config.pane_select_font_size = sizes.pane_select;
        Self {
            config: Arc::new(config),
            generation: self.generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes() -> NativeFontSizes {
        NativeFontSizes {
            body: 5.4,
            title: 5.4,
            command_palette: 6.3,
            char_select: 8.1,
            pane_select: 16.2,
        }
    }

    #[test]
    fn copy_preserves_original_styles_colors_and_generation() {
        let mut config = crate::Config::default_config();
        config.window_frame.font = Some(config.font.clone());
        config.command_palette_font = Some(config.font.clone());
        config.char_select_font = Some(config.font.clone());
        config.pane_select_font = Some(config.font.clone());
        let original = crate::ConfigHandle {
            config: Arc::new(config),
            generation: 7,
        };
        let copy = original.with_native_font_sizes(sizes());
        assert_eq!(NativeFontSizes::from(&*copy), sizes());
        assert_eq!(original.font_size, crate::DEFAULT_FONT_SIZE);
        assert_eq!(original.window_frame.font_size, None);
        assert_eq!(copy.generation(), original.generation());
        assert_eq!(copy.font, original.font);
        assert_eq!(copy.window_frame.font, original.window_frame.font);
        assert_eq!(
            copy.window_frame.active_titlebar_bg,
            original.window_frame.active_titlebar_bg
        );
        assert_eq!(copy.command_palette_font, original.command_palette_font);
        assert_eq!(copy.char_select_font, original.char_select_font);
        assert_eq!(copy.pane_select_font, original.pane_select_font);
        assert_eq!(
            copy.command_palette_fg_color,
            original.command_palette_fg_color
        );
        assert_eq!(
            copy.command_palette_line_height,
            original.command_palette_line_height
        );
    }

    #[test]
    #[ignore = "invoked in an isolated process by reload_and_overridden_config_inherit_typed_defaults"]
    fn isolated_reload() {
        crate::common_init(None, &[], true).unwrap();
        set_native_font_defaults(sizes());
        crate::reload();
        assert_eq!(NativeFontSizes::from(&*crate::configuration()), sizes());
        let overridden =
            crate::overridden_config(&wezterm_dynamic::Value::Object(Default::default())).unwrap();
        assert_eq!(NativeFontSizes::from(&*overridden), sizes());
        assert_eq!(
            NativeFontSizes::from(&*crate::ConfigHandle::default_config()),
            sizes()
        );
    }

    #[test]
    fn reload_and_overridden_config_inherit_typed_defaults() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "font_sizes::tests::isolated_reload",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored;"));
    }
}
