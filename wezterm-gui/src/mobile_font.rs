//! Native point-size and metric preparation; no window or GPU is created.

use crate::utilsprites::RenderMetrics;
use config::{ConfigHandle, NativeFontSizes};
use std::rc::Rc;
use wezterm_font::FontConfiguration;

pub(crate) fn prepare(
    config: &ConfigHandle,
    dpi: usize,
    size: NativeFontSizes,
) -> anyhow::Result<(ConfigHandle, Rc<FontConfiguration>)> {
    let config = config.with_native_font_sizes(size);
    let height = config.font_size * dpi as f64 / 72.0;
    // Preserve TermWindow::apply_scale_change's existing 2 pixel minimum.
    anyhow::ensure!(
        height.is_finite() && height >= 2.0,
        "font height is not usable"
    );
    let fonts = Rc::new(FontConfiguration::new(Some(config.clone()), dpi)?);
    let metrics = RenderMetrics::new(&fonts)?;
    anyhow::ensure!(
        metrics.cell_size.width > 0 && metrics.cell_size.height > 0,
        "font cells must be nonzero"
    );
    for font in [
        fonts.title_font()?,
        fonts.command_palette_font()?,
        fonts.char_select_font()?,
        fonts.pane_select_font()?,
    ] {
        let metrics = font.metrics();
        anyhow::ensure!(
            metrics.cell_width.get().is_finite()
                && metrics.cell_width.get() > 0.0
                && metrics.cell_height.get().is_finite()
                && metrics.cell_height.get() > 0.0,
            "GUI font cells must be finite and nonzero"
        );
    }
    let bar_height = crate::TermWindow::tab_bar_pixel_height_impl(&config, &fonts, &metrics)?;
    anyhow::ensure!(
        bar_height.is_finite() && bar_height > 0.0,
        "tabbar height is not usable"
    );
    Ok((config, fonts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ConfigHandle {
        ConfigHandle::default_config()
    }

    // The entities keep their default proportions to the body size.
    fn sizes(body: f64) -> NativeFontSizes {
        NativeFontSizes {
            body,
            title: body,
            command_palette: body / config::DEFAULT_FONT_SIZE
                * config::DEFAULT_COMMAND_PALETTE_FONT_SIZE,
            char_select: body / config::DEFAULT_FONT_SIZE * config::DEFAULT_CHAR_SELECT_FONT_SIZE,
            pane_select: body / config::DEFAULT_FONT_SIZE * config::DEFAULT_PANE_SELECT_FONT_SIZE,
        }
    }

    #[test]
    fn actual_native_metrics_shrink_and_reset_without_accumulating_scale() {
        for dpi in [160, 320, 420, 640] {
            let (_, default) = prepare(&config(), dpi, sizes(5.4)).unwrap();
            let default_metrics = RenderMetrics::new(&default).unwrap();
            default.change_scaling(2.0, dpi);
            let (smaller_config, smaller) = prepare(&default.config(), dpi, sizes(3.6)).unwrap();
            assert_eq!(smaller.get_font_scale(), 1.0);
            let smaller_metrics = RenderMetrics::new(&smaller).unwrap();
            assert!(smaller_metrics.cell_size.height < default_metrics.cell_size.height);
            let (_, reset) = prepare(&smaller_config, dpi, sizes(5.4)).unwrap();
            assert_eq!(reset.get_font_scale(), 1.0);
            assert_eq!(
                RenderMetrics::new(&reset).unwrap().cell_size,
                default_metrics.cell_size
            );
        }
    }

    fn title_and_bar(fonts: &Rc<FontConfiguration>) -> (f64, f32) {
        let title_height = fonts.title_font().unwrap().metrics().cell_height.get();
        let config = fonts.config();
        assert!(config.use_fancy_tab_bar);
        let metrics = RenderMetrics::new(fonts).unwrap();
        let bar_height =
            crate::TermWindow::tab_bar_pixel_height_impl(&config, fonts, &metrics).unwrap();
        (title_height, bar_height)
    }

    fn assert_metric_ratio(actual: f64, reference: f64, ratio: f64) {
        // Each native FreeType size can quantize at the pixel boundary;
        // allow one pixel per independently sized measurement, not a size cap.
        assert!((actual - reference * ratio).abs() <= 1.0 + ratio);
    }

    #[test]
    #[cfg(not(target_os = "android"))]
    fn actual_native_title_and_tabbar_follow_size_choice_and_density() {
        let mut at_160 = None;
        for dpi in [160, 320, 420, 640] {
            let old = Rc::new(FontConfiguration::new(Some(config()), dpi).unwrap());
            let (old_title, old_bar) = title_and_bar(&old);
            let (default_config, default) = prepare(&config(), dpi, sizes(5.4)).unwrap();
            let default_metrics = title_and_bar(&default);
            assert!(default_metrics.0 < old_title);
            assert!(default_metrics.1 < old_bar);
            assert_metric_ratio(
                default_metrics.0,
                old_title,
                // The unchanged native title default is 10 pt on Windows, 12 elsewhere.
                default_config.font_size / if cfg!(windows) { 10.0 } else { 12.0 },
            );
            if let Some(reference) = at_160 {
                assert_metric_ratio(default_metrics.0, reference, dpi as f64 / 160.0);
            } else {
                at_160 = Some(default_metrics.0);
            }

            let (smaller_config, smaller) = prepare(&default_config, dpi, sizes(3.6)).unwrap();
            let smaller_metrics = title_and_bar(&smaller);
            assert!(smaller_metrics.0 < default_metrics.0);
            assert!(smaller_metrics.1 < default_metrics.1);
            assert_metric_ratio(smaller_metrics.0, default_metrics.0, 8.0 / 12.0);

            // The existing config-change path drops the cached native title.
            let cached_title = default.title_font().unwrap();
            default.config_changed(&smaller_config).unwrap();
            assert!(!Rc::ptr_eq(&cached_title, &default.title_font().unwrap()));
            assert_eq!(title_and_bar(&default), smaller_metrics);
            let new_window_fonts =
                Rc::new(FontConfiguration::new(Some(smaller_config.clone()), dpi).unwrap());
            assert_eq!(title_and_bar(&new_window_fonts), smaller_metrics);

            let (_, reset) = prepare(&smaller_config, dpi, sizes(5.4)).unwrap();
            assert_eq!(title_and_bar(&reset), default_metrics);
        }
    }

    fn all_entity_fonts(fonts: &FontConfiguration) -> [Rc<wezterm_font::LoadedFont>; 5] {
        [
            fonts.default_font().unwrap(),
            fonts.title_font().unwrap(),
            fonts.command_palette_font().unwrap(),
            fonts.char_select_font().unwrap(),
            fonts.pane_select_font().unwrap(),
        ]
    }

    fn all_entity_heights(fonts: &FontConfiguration) -> [f64; 5] {
        all_entity_fonts(fonts).map(|font| font.metrics().cell_height.get())
    }

    #[test]
    fn every_native_entity_follows_size_choice_density_reset_and_cache_changes() {
        let mut at_160 = None;
        for dpi in [160, 320, 420, 640] {
            let (default_config, default) = prepare(&config(), dpi, sizes(5.4)).unwrap();
            let default_heights = all_entity_heights(&default);
            let cached = all_entity_fonts(&default);
            for (cached, repeated) in cached.iter().zip(all_entity_fonts(&default)) {
                assert!(Rc::ptr_eq(cached, &repeated));
            }
            if let Some(reference) = at_160 {
                for (actual, reference) in default_heights.iter().zip(reference) {
                    assert_metric_ratio(*actual, reference, dpi as f64 / 160.0);
                }
            } else {
                at_160 = Some(default_heights);
            }
            // Pane labels retain their intentional relative emphasis.
            assert!(default_heights[4] > default_heights[3]);
            assert!(default_heights[3] > default_heights[2]);
            #[cfg(not(target_os = "android"))]
            {
                let old = FontConfiguration::new(Some(config()), dpi).unwrap();
                let old_heights = all_entity_heights(&old);
                for index in [2, 3, 4] {
                    assert!(default_heights[index] < old_heights[index]);
                    assert_metric_ratio(default_heights[index], old_heights[index], 72.0 / 160.0);
                }
            }
            let (smaller_config, smaller) = prepare(&default_config, dpi, sizes(3.6)).unwrap();
            let smaller_heights = all_entity_heights(&smaller);
            for (smaller, default) in smaller_heights.iter().zip(default_heights) {
                assert!(*smaller < default);
                assert_metric_ratio(*smaller, default, 8.0 / 12.0);
            }
            default.config_changed(&smaller_config).unwrap();
            for (old, new) in cached.iter().zip(all_entity_fonts(&default)) {
                assert!(!Rc::ptr_eq(old, &new));
            }
            assert_eq!(all_entity_heights(&default), smaller_heights);
            let new_window = FontConfiguration::new(Some(smaller_config.clone()), dpi).unwrap();
            assert_eq!(all_entity_heights(&new_window), smaller_heights);
            let (reset_config, reset) = prepare(&smaller_config, dpi, sizes(5.4)).unwrap();
            assert_eq!(all_entity_heights(&reset), default_heights);
            default.config_changed(&reset_config).unwrap();
            assert_eq!(all_entity_heights(&default), default_heights);
        }
    }

    #[test]
    fn native_constraints_reject_unusable_density_and_huge_sizes() {
        assert!(prepare(&config(), 0, sizes(5.4)).is_err());
        assert!(prepare(&config(), 160, sizes(f64::MAX)).is_err());
        assert!(prepare(&config(), 160, sizes(1.8)).is_ok());
    }
}
