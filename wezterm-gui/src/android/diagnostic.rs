//! Debug-only diagnostic applet.
//!
//! A termwiz program draws a fixed grid of styled, wide, combining and
//! fallback glyphs into a mux pane, so the Android surface shows what
//! `TermWindow` renders without any shell or remote domain.

use mux::termwiztermtab::TermWizTerminal;
use termwiz::cell::{AttributeChange, CellAttributes, Intensity, Underline};
use termwiz::color::{AnsiColor, ColorAttribute};
use termwiz::input::InputEvent;
use termwiz::surface::{Change, CursorShape, CursorVisibility};
use termwiz::terminal::Terminal;
use wezterm_term::TerminalSize;

/// Literal rows of the grid, row 1 excepted (it carries the live size and
/// redraw count).  Device evidence compares rendered text against these.
pub const ROWS: [&str; 8] = [
    "WezTerm Android surface diagnostic",
    "cols=<cols> rows=<rows> redraw=<n>",
    "attrs: bold italic underline reverse strike",
    "colors: red green yellow blue magenta cyan on-blue",
    "wide: 漢字 テスト ｗｉｄｅ",
    "combining: e\u{301} n\u{303} a\u{30A} o\u{308}",
    "fallback: 🚀 ⇒ ✓ ★ ▶ █ ─ │",
    "cursor> ",
];

/// Create the applet's mux window and run it until its pane goes away.
pub async fn run() -> anyhow::Result<()> {
    let size = TerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 80 * 8,
        pixel_height: 24 * 16,
        dpi: 0,
    };
    mux::termwiztermtab::run(size, None, draw_until_closed, None).await
}

fn draw_until_closed(mut term: TermWizTerminal) -> anyhow::Result<()> {
    term.no_grab_mouse_in_raw_mode();
    term.set_raw_mode()?;
    let mut size = term.get_screen_size()?;
    let mut redraws = 0u64;
    loop {
        redraws += 1;
        term.render(&grid(size.cols, size.rows, redraws))?;
        term.flush()?;
        if let Some(InputEvent::Resized { cols, rows }) = term.poll_input(None)? {
            size.cols = cols;
            size.rows = rows;
        }
    }
}

fn fg(color: AnsiColor) -> Change {
    Change::Attribute(AttributeChange::Foreground(ColorAttribute::from(color)))
}

fn reset() -> Change {
    Change::AllAttributes(CellAttributes::default())
}

fn text(s: &str) -> Change {
    Change::Text(s.to_string())
}

fn grid(cols: usize, rows: usize, redraws: u64) -> Vec<Change> {
    vec![
        Change::ClearScreen(ColorAttribute::Default),
        Change::CursorVisibility(CursorVisibility::Visible),
        Change::CursorShape(CursorShape::SteadyBlock),
        Change::Attribute(AttributeChange::Intensity(Intensity::Bold)),
        text(ROWS[0]),
        reset(),
        text(&format!("\r\ncols={cols} rows={rows} redraw={redraws}\r\n")),
        text("attrs: "),
        Change::Attribute(AttributeChange::Intensity(Intensity::Bold)),
        text("bold"),
        reset(),
        text(" "),
        Change::Attribute(AttributeChange::Italic(true)),
        text("italic"),
        reset(),
        text(" "),
        Change::Attribute(AttributeChange::Underline(Underline::Single)),
        text("underline"),
        reset(),
        text(" "),
        Change::Attribute(AttributeChange::Reverse(true)),
        text("reverse"),
        reset(),
        text(" "),
        Change::Attribute(AttributeChange::StrikeThrough(true)),
        text("strike"),
        reset(),
        text("\r\ncolors: "),
        fg(AnsiColor::Red),
        text("red "),
        fg(AnsiColor::Green),
        text("green "),
        fg(AnsiColor::Yellow),
        text("yellow "),
        fg(AnsiColor::Blue),
        text("blue "),
        fg(AnsiColor::Fuchsia),
        text("magenta "),
        fg(AnsiColor::Aqua),
        text("cyan "),
        reset(),
        Change::Attribute(AttributeChange::Background(ColorAttribute::from(
            AnsiColor::Navy,
        ))),
        text("on-blue"),
        reset(),
        text(&format!(
            "\r\n{}\r\n{}\r\n{}\r\n{}",
            ROWS[4], ROWS[5], ROWS[6], ROWS[7]
        )),
    ]
}
