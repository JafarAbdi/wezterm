//! A fallback font found when the font list was built and gone when the
//! shaper first loads it: the cluster is shaped as a placeholder, and the
//! failure is logged without the text on Android.

use std::sync::Mutex;
use wezterm_bidi::Direction;
use wezterm_font::locator::{FontDataHandle, FontDataSource, FontOrigin};
use wezterm_font::parser::ParsedFont;

struct Records(Mutex<Vec<(log::Level, String)>>);

impl log::Log for Records {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let mut records = self.0.lock().unwrap();
        records.push((record.level(), record.args().to_string()));
    }

    fn flush(&self) {}
}

static RECORDS: Records = Records(Mutex::new(Vec::new()));

fn parse(path: &std::path::Path) -> ParsedFont {
    ParsedFont::from_locator(&FontDataHandle {
        source: FontDataSource::OnDisk(path.to_path_buf()),
        index: 0,
        variation: 0,
        origin: FontOrigin::FontDirs,
        coverage: None,
    })
    .unwrap()
}

#[test]
fn a_fallback_font_that_fails_to_load_shapes_placeholders_where_the_text_was() {
    log::set_logger(&RECORDS).unwrap();
    log::set_max_level(log::LevelFilter::Trace);

    let dir = std::env::temp_dir().join(format!("wezterm-font-fallback-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let primary = dir.join("JetBrainsMono-Regular.ttf");
    let fallback = dir.join("SymbolsNerdFontMono-Regular.ttf");
    std::fs::write(
        &primary,
        include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"),
    )
    .unwrap();
    std::fs::write(
        &fallback,
        include_bytes!("../../assets/fonts/SymbolsNerdFontMono-Regular.ttf"),
    )
    .unwrap();
    let fonts = [parse(&primary), parse(&fallback)];
    std::fs::remove_file(&fallback).unwrap();

    let shaper = wezterm_font::shaper::new_shaper(&config::configuration(), &fonts).unwrap();
    let mut no_glyphs = vec![];
    let text = "a\u{1f6f0}\u{1f6f1}";
    let glyphs = shaper
        .shape(
            text,
            10.,
            72,
            &mut no_glyphs,
            None,
            Direction::LeftToRight,
            None,
            None,
        )
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    let shown: Vec<_> = glyphs
        .iter()
        .map(|g| (g.font_idx, g.only_char, g.cluster))
        .collect();
    assert_eq!(
        shown,
        [
            (0, Some('a'), 0),
            (0, Some('\u{fffd}'), 1),
            (0, Some('\u{fffd}'), 5)
        ]
    );

    let records = RECORDS.0.lock().unwrap();
    let errors: Vec<&str> = records
        .iter()
        .filter(|(level, _)| *level == log::Level::Error)
        .map(|(_, message)| message.as_str())
        .collect();
    if cfg!(target_os = "android") {
        assert_eq!(
            errors,
            ["no fallback font could shape 8 bytes of text; showing placeholders"]
        );
        let holding = records
            .iter()
            .filter(|(_, message)| {
                ["\u{1f6f0}", "\u{1f6f1}", "1f6f0", "1f6f1"]
                    .iter()
                    .any(|needle| message.contains(needle))
            })
            .count();
        assert_eq!(holding, 0, "records holding the text");
    } else {
        let quoted: Vec<bool> = errors
            .iter()
            .map(|e| e.ends_with(" for \"\u{1f6f0}\""))
            .collect();
        assert_eq!(quoted, [true], "{errors:?}");
    }
}
