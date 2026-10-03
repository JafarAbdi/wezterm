//! Placeholders for text no fallback font could shape take the cells the
//! terminal gave that text, so the rest of the row stays in its cells.

use std::sync::atomic::{AtomicUsize, Ordering};
use termwiz::cell::{Cell, CellAttributes};
use termwiz::surface::Line;
use wezterm_bidi::Direction;
use wezterm_font::locator::{FontDataHandle, FontDataSource, FontOrigin};
use wezterm_font::parser::ParsedFont;
use wezterm_font::shaper::{FontShaper, PresentationWidth};

/// JetBrains Mono, then a fallback font whose file is gone by the time
/// the shaper first loads it.
fn shaper_with_a_dead_fallback() -> Box<dyn FontShaper> {
    static SHAPERS: AtomicUsize = AtomicUsize::new(0);
    let primary = ParsedFont::from_locator(&FontDataHandle {
        source: FontDataSource::BuiltIn {
            name: "JetBrainsMono-Regular.ttf",
            data: include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"),
        },
        index: 0,
        variation: 0,
        origin: FontOrigin::BuiltIn,
        coverage: None,
    })
    .unwrap();
    let path = std::env::temp_dir().join(format!(
        "wezterm-font-placeholder-{}-{}.ttf",
        std::process::id(),
        SHAPERS.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &path,
        include_bytes!("../../assets/fonts/SymbolsNerdFontMono-Regular.ttf"),
    )
    .unwrap();
    let fallback = ParsedFont::from_locator(&FontDataHandle {
        source: FontDataSource::OnDisk(path.clone()),
        index: 0,
        variation: 0,
        origin: FontOrigin::FontDirs,
        coverage: None,
    })
    .unwrap();
    std::fs::remove_file(&path).unwrap();
    wezterm_font::shaper::new_shaper(&config::configuration(), &[primary, fallback]).unwrap()
}

/// Shape `line` as the GUI shapes one of its clusters, as
/// `(glyph, byte offset, cells)` per glyph.
fn shape_line(line: &Line) -> Vec<(Option<char>, u32, u8)> {
    let clusters = line.cluster(None);
    let [cluster] = clusters.as_slice() else {
        panic!("one cluster expected, got {}", clusters.len());
    };
    let width = PresentationWidth::with_cluster(cluster);
    shaper_with_a_dead_fallback()
        .shape(
            &cluster.text,
            10.,
            72,
            &mut vec![],
            Some(cluster.presentation),
            Direction::LeftToRight,
            None,
            Some(&width),
        )
        .unwrap()
        .iter()
        .map(|g| (g.only_char, g.cluster, g.num_cells))
        .collect()
}

#[test]
fn a_wide_character_keeps_its_two_cells() {
    let line = Line::from_text("中a", &CellAttributes::default(), 0, None);
    assert_eq!(
        shape_line(&line),
        [(Some('\u{fffd}'), 0, 2), (Some('a'), 3, 1)]
    );
}

#[test]
fn a_run_of_wide_characters_after_ascii_keeps_each_ones_cells() {
    let line = Line::from_text("a中\u{20000}b", &CellAttributes::default(), 0, None);
    assert_eq!(
        shape_line(&line),
        [
            (Some('a'), 0, 1),
            (Some('\u{fffd}'), 1, 2),
            (Some('\u{fffd}'), 4, 2),
            (Some('b'), 8, 1)
        ]
    );
}

#[test]
fn a_row_of_wide_characters_wider_than_255_cells_keeps_every_cell() {
    let line = Line::from_text(&"中".repeat(200), &CellAttributes::default(), 0, None);
    let cells: Vec<u8> = shape_line(&line)
        .iter()
        .map(|&(_, _, cells)| cells)
        .collect();
    assert_eq!(cells, [2; 200]);
}

#[test]
fn the_width_the_terminal_gave_the_text_wins_over_unicode() {
    let attrs = CellAttributes::default();
    let line = Line::from_cells(
        vec![
            Cell::new_grapheme_with_width("中", 1, attrs.clone()),
            Cell::new('a', attrs),
        ],
        0,
    );
    assert_eq!(
        shape_line(&line),
        [(Some('\u{fffd}'), 0, 1), (Some('a'), 3, 1)]
    );
}

#[test]
fn without_the_terminal_widths_a_wide_emoji_keeps_its_unicode_width() {
    let glyphs = shaper_with_a_dead_fallback()
        .shape(
            "\u{1f600}a",
            10.,
            72,
            &mut vec![],
            None,
            Direction::LeftToRight,
            None,
            None,
        )
        .unwrap();
    let shown: Vec<_> = glyphs
        .iter()
        .map(|g| (g.only_char, g.cluster, g.num_cells))
        .collect();
    assert_eq!(shown, [(Some('\u{fffd}'), 0, 2), (Some('a'), 4, 1)]);
}
