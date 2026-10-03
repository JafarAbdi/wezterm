//! Debug-only diagnostic applet.
//!
//! A termwiz program draws a fixed grid of styled, wide, combining and
//! fallback glyphs into a mux pane, so the Android surface shows what
//! `TermWindow` renders without any shell or remote domain.

use crate::termwindow::TermWindowNotif;
use ::window::{Connection, ConnectionOps, WindowOps};
use anyhow::Context;
use config::keyassignment::{ClipboardPasteSource, KeyAssignment};
use mux::tab::{PaneNode, TabId};
use mux::termwiztermtab::TermWizTerminal;
use mux::Mux;
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
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

static WINDOWS_OPENED: AtomicUsize = AtomicUsize::new(0);

/// Open one more applet window.  Each is its own mux window with its own
/// logical GUI window; the first one is titled `diagnostic 1`.
pub fn open_window() {
    let number = WINDOWS_OPENED.fetch_add(1, Ordering::SeqCst) + 1;
    promise::spawn::spawn(async move {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 80 * 8,
            pixel_height: 24 * 16,
            dpi: 0,
        };
        let draw = move |term| draw_until_closed(term, number);
        if let Err(err) = mux::termwiztermtab::run(size, None, draw, None).await {
            log::error!("diagnostic applet {number} ended: {err:#}");
        }
    })
    .detach();
}

/// Paste the clipboard into the active pane of the window bound to the
/// surface, through the same key assignment a paste shortcut performs.
pub fn paste_into_bound_window() -> anyhow::Result<()> {
    let (bound, pane) = super::bound_pane()?;
    bound.notify(TermWindowNotif::PerformAssignment {
        pane_id: pane.pane_id(),
        assignment: KeyAssignment::PasteFrom(ClipboardPasteSource::Clipboard),
        tx: None,
    });
    Ok(())
}

thread_local! {
    /// The pane tree `snapshot_bound_tab` kept, with its tab.
    static TAB_SNAPSHOT: RefCell<Option<(TabId, PaneNode)>> = const { RefCell::new(None) };
}

/// Keep the pane tree, with its sizes, of the bound window's active tab.
pub fn snapshot_bound_tab() -> anyhow::Result<()> {
    let (_, pane) = super::bound_pane()?;
    let mux = Mux::get();
    let (_, _, tab_id) = mux
        .resolve_pane_id(pane.pane_id())
        .context("the bound pane is in no tab")?;
    let tab = mux.get_tab(tab_id).context("no such tab")?;
    TAB_SNAPSHOT.set(Some((tab_id, tab.codec_pane_tree())));
    Ok(())
}

/// Apply the kept pane tree to its tab through `Tab::sync_with_pane_tree`,
/// as a client resync applies a pane tree the laptop sent: panes and tab
/// take the tree's sizes, and the panes tell the laptop.
pub fn apply_bound_tab_snapshot() -> anyhow::Result<()> {
    let (tab_id, tree) = TAB_SNAPSHOT.take().context("no tab snapshot")?;
    let mux = Mux::get();
    let tab = mux.get_tab(tab_id).context("the snapshot's tab is gone")?;
    let size = tree.root_size().context("an empty pane tree")?;
    let panes: Vec<_> = tab
        .iter_panes_ignoring_zoom()
        .into_iter()
        .map(|p| p.pane)
        .collect();
    tab.sync_with_pane_tree(size, tree, |entry| {
        panes
            .iter()
            .find(|pane| pane.pane_id() == entry.pane_id)
            .cloned()
            .expect("the snapshot names the tab's panes")
    });
    Ok(())
}

/// Start a clipboard read on the window bound to the surface.
pub fn read_bound_clipboard() -> anyhow::Result<promise::Future<String>> {
    let bound = Connection::get()
        .and_then(|conn| conn.bound_window())
        .context("no window is bound")?;
    Ok(bound.get_clipboard(::window::Clipboard::Clipboard))
}

fn draw_until_closed(mut term: TermWizTerminal, number: usize) -> anyhow::Result<()> {
    term.no_grab_mouse_in_raw_mode();
    term.set_raw_mode()?;
    let mut size = term.get_screen_size()?;
    let mut redraws = 0u64;
    let mut pasted = String::new();
    loop {
        redraws += 1;
        term.render(&grid(number, size.cols, size.rows, redraws, &pasted))?;
        term.flush()?;
        match term.poll_input(None)? {
            Some(InputEvent::Resized { cols, rows }) => {
                size.cols = cols;
                size.rows = rows;
            }
            Some(InputEvent::Paste(text)) => pasted = text,
            _ => {}
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

/// The first window draws exactly [`ROWS`]; later windows append their
/// number to row 0, and pasted text follows the prompt of the last row.
fn grid(number: usize, cols: usize, rows: usize, redraws: u64, pasted: &str) -> Vec<Change> {
    let heading = match number {
        1 => ROWS[0].to_string(),
        _ => format!("{} #{number}", ROWS[0]),
    };
    vec![
        Change::Title(format!("diagnostic {number}")),
        Change::ClearScreen(ColorAttribute::Default),
        Change::CursorVisibility(CursorVisibility::Visible),
        Change::CursorShape(CursorShape::SteadyBlock),
        Change::Attribute(AttributeChange::Intensity(Intensity::Bold)),
        text(&heading),
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
            "\r\n{}\r\n{}\r\n{}\r\n{}{pasted}",
            ROWS[4], ROWS[5], ROWS[6], ROWS[7]
        )),
    ]
}
