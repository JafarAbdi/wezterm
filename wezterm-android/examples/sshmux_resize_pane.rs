//! Test fixture: a laptop client of a local mux server that resizes one
//! pane, as a laptop GUI attached to that server does when its window
//! changes size.
//!
//! `sshmux_resize_pane <rows> <cols>` sends the `Resize` request for the
//! pane `WEZTERM_PANE` names to the server at `WEZTERM_UNIX_SOCKET` and
//! returns once the server acknowledged it.  The server resizes the pane
//! and its tab and tells every attached client.

use anyhow::{Context, bail};
use codec::{ListPanes, Pdu, Resize};
use mux::tab::{PaneEntry, PaneNode};
use std::os::unix::net::UnixStream;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [rows, cols] = args.as_slice() else {
        bail!("usage: sshmux_resize_pane <rows> <cols>");
    };
    let (rows, cols): (usize, usize) = (rows.parse()?, cols.parse()?);
    let pane_id: usize = std::env::var("WEZTERM_PANE")?.parse()?;
    let mut server = UnixStream::connect(std::env::var("WEZTERM_UNIX_SOCKET")?)?;

    let tabs = match request(&mut server, 1, Pdu::ListPanes(ListPanes {}))? {
        Pdu::ListPanesResponse(response) => response.tabs,
        other => bail!("unexpected answer to ListPanes: {other:?}"),
    };
    let pane = tabs
        .iter()
        .find_map(|tab| leaf(tab, pane_id))
        .with_context(|| format!("the server has no pane {pane_id}"))?;
    let mut size = pane.size;
    size.pixel_width = size.pixel_width / size.cols.max(1) * cols;
    size.pixel_height = size.pixel_height / size.rows.max(1) * rows;
    size.rows = rows;
    size.cols = cols;
    let resize = Resize {
        containing_tab_id: pane.tab_id,
        pane_id,
        size,
    };
    match request(&mut server, 2, Pdu::Resize(resize))? {
        Pdu::UnitResponse(_) => Ok(()),
        other => bail!("unexpected answer to Resize: {other:?}"),
    }
}

/// Send `pdu` and return the server's answer to it.
fn request(server: &mut UnixStream, serial: u64, pdu: Pdu) -> anyhow::Result<Pdu> {
    pdu.encode(&mut *server, serial)?;
    loop {
        let answer = Pdu::decode(&mut *server)?;
        if answer.serial == serial {
            return Ok(answer.pdu);
        }
    }
}

fn leaf(node: &PaneNode, pane_id: usize) -> Option<&PaneEntry> {
    match node {
        PaneNode::Leaf(entry) if entry.pane_id == pane_id => Some(entry),
        PaneNode::Split { left, right, .. } => leaf(left, pane_id).or_else(|| leaf(right, pane_id)),
        _ => None,
    }
}
