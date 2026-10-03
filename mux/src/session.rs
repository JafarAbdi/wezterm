//! Saves the mux's local windows, tabs, panes and scrollback to SQLite and
//! rebuilds them when the mux server starts again after a crash or reboot.
//!
//! Processes do not survive a crash. A restored pane gets a fresh shell in
//! its old cwd with its old scrollback above the prompt, and the program that
//! was running is resumed (`Relaunch::Run`) if it published the command that
//! resumes it in the `WEZTERM_RESUME` user var, and otherwise typed at the
//! prompt (`Relaunch::Type`).

use crate::domain::LocalDomain;
use crate::pane::{CachePolicy, Pane, PaneId};
use crate::tab::{PaneEntry, PaneNode, Tab};
use crate::{Mux, MuxNotification};
use anyhow::Context;
use crossbeam::channel::{bounded, Receiver, RecvTimeoutError, TrySendError};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use termwiz::surface::SequenceNo;
use wezterm_term::TerminalSize;

const KEEP_SNAPSHOTS: i64 = 20;
/// Empirical: long enough to coalesce a burst of layout changes or output
/// into one save, short enough that a crash loses about a second of work.
const SAVE_QUIET: Duration = Duration::from_secs(1);
/// Empirical: bounds how stale the saved scrollback of a pane that never
/// stops printing can get.
const SAVE_MAX_DELAY: Duration = Duration::from_secs(10);
const RESUME_VAR: &str = "WEZTERM_RESUME";

const SCHEMA: &str = "
BEGIN;
CREATE TABLE snapshot (
  id          INTEGER PRIMARY KEY,
  saved_at_ms INTEGER NOT NULL,
  layout      TEXT    NOT NULL
) STRICT;
CREATE TABLE text (
  hash TEXT PRIMARY KEY,
  data BLOB NOT NULL
) STRICT;
CREATE TABLE snapshot_text (
  snapshot_id INTEGER NOT NULL REFERENCES snapshot (id) ON DELETE CASCADE,
  hash        TEXT    NOT NULL REFERENCES text (hash),
  PRIMARY KEY (snapshot_id, hash)
) STRICT;
CREATE INDEX snapshot_text_hash ON snapshot_text (hash);
PRAGMA user_version = 1;
COMMIT;
";
const SCHEMA_VERSION: i64 = 1;

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Snapshot {
    windows: Vec<WindowState>,
    panes: HashMap<PaneId, PaneState>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct WindowState {
    workspace: String,
    title: String,
    active_tab: usize,
    tabs: Vec<TabState>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct TabState {
    title: String,
    size: TerminalSize,
    tree: PaneNode,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct PaneState {
    domain: String,
    /// Content hash of the scrollback, a key into the `text` table.
    text: Option<String>,
    relaunch: Relaunch,
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
enum Relaunch {
    Shell,
    Run(String),
    Type(String),
}

pub struct Store {
    conn: Connection,
    /// Held for the store's lifetime so a second server on the same file
    /// cannot restore and save alongside this one.
    _owner: std::fs::File,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let owner = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        owner
            .try_lock()
            .with_context(|| format!("{} is owned by another mux server", path.display()))?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        anyhow::ensure!(
            mode == "wal",
            "{} is in {mode} journal mode",
            path.display()
        );
        conn.execute_batch(
            "PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => conn.execute_batch(SCHEMA)?,
            SCHEMA_VERSION => {}
            v => anyhow::bail!(
                "{} has schema {v}, expected {SCHEMA_VERSION}",
                path.display()
            ),
        }
        Ok(Self {
            conn,
            _owner: owner,
        })
    }

    /// The newest snapshot and the scrollback text it references.
    fn newest(&self) -> anyhow::Result<Option<(Snapshot, HashMap<String, Vec<u8>>)>> {
        let row: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, layout FROM snapshot ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, layout)) = row else {
            return Ok(None);
        };
        let snapshot: Snapshot =
            serde_json::from_str(&layout).with_context(|| format!("parsing snapshot {id}"))?;
        let mut stmt = self.conn.prepare(
            "SELECT text.hash, text.data FROM snapshot_text JOIN text USING (hash) \
             WHERE snapshot_id = ?1",
        )?;
        let mut texts = HashMap::new();
        for row in stmt.query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })? {
            let (hash, data) = row?;
            texts.insert(hash, zstd::decode_all(&data[..])?);
        }
        Ok(Some((snapshot, texts)))
    }

    fn insert(
        &mut self,
        layout: &str,
        new_texts: &HashMap<String, Vec<u8>>,
        refs: &BTreeSet<String>,
    ) -> anyhow::Result<()> {
        let saved_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (hash, raw) in new_texts {
            tx.execute(
                "INSERT INTO text (hash, data) VALUES (?1, ?2) ON CONFLICT (hash) DO NOTHING",
                params![hash, zstd::encode_all(&raw[..], 0)?],
            )?;
        }
        tx.execute(
            "INSERT INTO snapshot (saved_at_ms, layout) VALUES (?1, ?2)",
            params![saved_at_ms, layout],
        )?;
        let id = tx.last_insert_rowid();
        for hash in refs {
            tx.execute(
                "INSERT INTO snapshot_text (snapshot_id, hash) VALUES (?1, ?2)",
                params![id, hash],
            )?;
        }
        tx.execute(
            "DELETE FROM snapshot WHERE id NOT IN \
             (SELECT id FROM snapshot ORDER BY id DESC LIMIT ?1)",
            [KEEP_SNAPSHOTS],
        )?;
        tx.execute(
            "DELETE FROM text WHERE hash NOT IN (SELECT hash FROM snapshot_text)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn leaves<'a>(node: &'a PaneNode, out: &mut Vec<&'a PaneEntry>) {
    match node {
        PaneNode::Empty => {}
        PaneNode::Split { left, right, .. } => {
            leaves(left, out);
            leaves(right, out);
        }
        PaneNode::Leaf(entry) => out.push(entry),
    }
}

/// Rebuilds the newest snapshot. Call before clients can attach.
pub async fn restore(store: &Store) -> anyhow::Result<()> {
    let Some((snapshot, texts)) = store.newest()? else {
        return Ok(());
    };
    let mux = Mux::get();
    for window_state in snapshot.windows {
        // Clients hear about the window when the builder drops, once it has tabs.
        let window = mux.new_empty_window(Some(window_state.workspace), None);
        let window_id = *window;
        let mut restored = 0;
        for tab_state in window_state.tabs {
            match restore_tab(&mux, tab_state, &snapshot.panes, &texts).await {
                Ok(tab) => {
                    mux.add_tab_to_window(&tab, window_id)?;
                    restored += 1;
                }
                Err(err) => log::error!("session: skipping a tab: {err:#}"),
            }
        }
        if restored == 0 {
            mux.kill_window(window_id);
            continue;
        }
        if let Some(mut window) = mux.get_window_mut(window_id) {
            window.set_title(&window_state.title);
            window.set_active_tab_idx_without_saving(window_state.active_tab.min(restored - 1));
        }
    }
    Ok(())
}

/// Spawns every pane of the tab before adding any to the mux, so a failure
/// leaves nothing behind: dropping an unadded LocalPane kills its process.
async fn restore_tab(
    mux: &Arc<Mux>,
    tab_state: TabState,
    states: &HashMap<PaneId, PaneState>,
    texts: &HashMap<String, Vec<u8>>,
) -> anyhow::Result<Arc<Tab>> {
    let mut entries = vec![];
    leaves(&tab_state.tree, &mut entries);
    let mut panes = HashMap::new();
    for entry in entries {
        let state = states
            .get(&entry.pane_id)
            .with_context(|| format!("pane {} is missing from the snapshot", entry.pane_id))?;
        let pane = spawn_pane(mux, entry, state, texts)
            .await
            .with_context(|| format!("restoring pane {}", entry.pane_id))?;
        panes.insert(entry.pane_id, (pane, &state.relaunch));
    }

    let mut started = vec![];
    for (pane, relaunch) in panes.values() {
        mux.add_pane(pane)?;
        let input = match relaunch {
            Relaunch::Run(cmd) => format!("{cmd}\r"),
            Relaunch::Type(cmd) => cmd.clone(),
            Relaunch::Shell => String::new(),
        };
        pane.writer().write_all(input.as_bytes())?;
        started.push(pane.pane_id());
    }

    let tab = Arc::new(Tab::new(&tab_state.size));
    tab.sync_with_pane_tree(tab_state.size, tab_state.tree, |entry| {
        let (pane, _) = panes
            .remove(&entry.pane_id)
            .expect("every leaf was spawned from this same tree");
        pane
    });
    tab.set_title(&tab_state.title);
    mux.add_tab_no_panes(&tab);
    log::info!("session: restored tab with panes {started:?}");
    Ok(tab)
}

async fn spawn_pane(
    mux: &Arc<Mux>,
    entry: &PaneEntry,
    state: &PaneState,
    texts: &HashMap<String, Vec<u8>>,
) -> anyhow::Result<Arc<dyn Pane>> {
    let domain = match mux.get_domain_by_name(&state.domain) {
        Some(domain) => domain,
        None => {
            log::warn!(
                "session: domain {:?} is gone, restoring pane {} in the default domain",
                state.domain,
                entry.pane_id
            );
            mux.default_domain()
        }
    };
    // Shells report OSC 7 cwds as file://<hostname>/path, which
    // Url::to_file_path rejects for any host but localhost.
    let cwd = entry
        .working_dir
        .as_ref()
        .filter(|u| u.url.scheme() == "file")
        .map(|u| {
            percent_encoding::percent_decode_str(u.url.path())
                .decode_utf8_lossy()
                .into_owned()
        });
    let pane = domain.spawn_pane(entry.size, None, cwd).await?;

    // The pty reader starts in add_pane, so this lands above the new shell's
    // first prompt.
    if let Some(text) = state.text.as_ref().and_then(|h| texts.get(h)) {
        let mut actions = vec![];
        termwiz::escape::parser::Parser::new().parse(text, |a| actions.push(a));
        pane.perform_actions(actions);
    }
    Ok(pane)
}

struct CapturedText {
    seqno: SequenceNo,
    hash: Option<String>,
}

/// Captures the mux into a snapshot. Scrollback is re-read only for panes
/// whose seqno moved since `cache`; the freshly read text lands in `new_texts`.
fn capture(
    mux: &Mux,
    cache: &mut HashMap<PaneId, CapturedText>,
    new_texts: &mut HashMap<String, Vec<u8>>,
) -> Snapshot {
    let mut snapshot = Snapshot {
        windows: vec![],
        panes: HashMap::new(),
    };
    let mut window_ids = mux.iter_windows();
    window_ids.sort();
    for window_id in window_ids {
        let Some((workspace, title, active_tab, tabs)) = mux.get_window(window_id).map(|w| {
            (
                w.get_workspace().to_string(),
                w.get_title().to_string(),
                w.get_active_tab_idx(),
                w.iter_tabs().cloned().collect::<Vec<_>>(),
            )
        }) else {
            continue;
        };
        let mut window_state = WindowState {
            workspace,
            title,
            active_tab: 0,
            tabs: vec![],
        };
        for (idx, tab) in tabs.iter().enumerate() {
            let tree = tab.codec_pane_tree();
            let mut entries = vec![];
            leaves(&tree, &mut entries);
            let panes: Option<Vec<_>> =
                entries.iter().map(|e| local_pane(mux, e.pane_id)).collect();
            let Some(panes) = panes.filter(|p| !p.is_empty()) else {
                continue;
            };
            for (entry, (pane, domain)) in entries.iter().zip(panes) {
                let state = PaneState {
                    domain,
                    text: pane_text(&*pane, cache, new_texts),
                    relaunch: relaunch(&*pane),
                };
                snapshot.panes.insert(entry.pane_id, state);
            }
            if idx == active_tab {
                window_state.active_tab = window_state.tabs.len();
            }
            window_state.tabs.push(TabState {
                title: tab.get_title(),
                size: tab.get_size(),
                tree,
            });
        }
        if !window_state.tabs.is_empty() {
            snapshot.windows.push(window_state);
        }
    }
    cache.retain(|id, _| snapshot.panes.contains_key(id));
    snapshot
}

/// The pane and its domain name, if the pane lives in a local domain and so
/// can be respawned. Tabs holding any other pane are not saved.
fn local_pane(mux: &Mux, pane_id: PaneId) -> Option<(Arc<dyn Pane>, String)> {
    let pane = mux.get_pane(pane_id)?;
    let domain = mux.get_domain(pane.domain_id())?;
    domain.downcast_ref::<LocalDomain>()?;
    Some((pane, domain.domain_name().to_string()))
}

fn pane_text(
    pane: &dyn Pane,
    cache: &mut HashMap<PaneId, CapturedText>,
    new_texts: &mut HashMap<String, Vec<u8>>,
) -> Option<String> {
    let seqno = pane.get_current_seqno();
    let previous = cache.get(&pane.pane_id());
    if let Some(prev) = previous {
        // A full-screen program's alternate screen is redrawn on relaunch;
        // keep the primary screen captured before it started.
        if prev.seqno == seqno || pane.is_alt_screen_active() {
            return prev.hash.clone();
        }
    } else if pane.is_alt_screen_active() {
        return None;
    }

    let dims = pane.get_dimensions();
    let bottom = dims.physical_top + dims.viewport_rows as isize;
    let (_, mut lines) = pane.get_lines(dims.scrollback_top..bottom);
    while lines.last().map_or(false, |l| l.is_whitespace()) {
        lines.pop();
    }
    let hash = if lines.is_empty() {
        None
    } else {
        match termwiz_funcs::lines_to_escapes(lines) {
            Ok(text) => {
                let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
                new_texts.insert(hash.clone(), text.into_bytes());
                Some(hash)
            }
            Err(err) => {
                log::error!("session: capturing pane {}: {err:#}", pane.pane_id());
                None
            }
        }
    };
    cache.insert(
        pane.pane_id(),
        CapturedText {
            seqno,
            hash: hash.clone(),
        },
    );
    hash
}

fn relaunch(pane: &dyn Pane) -> Relaunch {
    // The publisher clears the var when it exits (pi on quit, fish after every
    // command), so a set value names the program the pane is running even when
    // that program is the pane's own process, as in `fish -c 'pi; exec fish'`.
    if let Some(cmd) = pane.copy_user_vars().remove(RESUME_VAR) {
        if !cmd.is_empty() {
            return Relaunch::Run(cmd);
        }
    }
    let Some(fg) = pane.get_foreground_process_info(CachePolicy::AllowStale) else {
        return Relaunch::Shell;
    };
    // The pane's own process (normally the shell) is a direct child of the
    // server; anything the shell runs is not.
    if fg.ppid == std::process::id() {
        return Relaunch::Shell;
    }
    Relaunch::Type(shell_words::join(&fg.argv))
}

/// Saves the mux into `store` after changes settle, on a dedicated thread.
pub fn start_saver(mut store: Store) {
    let (wake, woken) = bounded(1);
    Mux::get().subscribe(move |n| {
        let relevant = matches!(
            n,
            MuxNotification::PaneOutput(_)
                | MuxNotification::PaneAdded(_)
                | MuxNotification::PaneRemoved(_)
                | MuxNotification::PaneFocused(_)
                | MuxNotification::WindowCreated(_)
                | MuxNotification::WindowRemoved(_)
                | MuxNotification::WindowInvalidated(_)
                | MuxNotification::WindowWorkspaceChanged(_)
                | MuxNotification::WindowTitleChanged { .. }
                | MuxNotification::TabAddedToWindow { .. }
                | MuxNotification::TabResized(_)
                | MuxNotification::TabTitleChanged { .. }
                | MuxNotification::WorkspaceRenamed { .. }
        );
        !(relevant && matches!(wake.try_send(()), Err(TrySendError::Disconnected(_))))
    });

    std::thread::Builder::new()
        .name("session-saver".into())
        .spawn(move || {
            let mut cache = HashMap::new();
            let mut last_layout = String::new();
            while settled(&woken) {
                let Some(mux) = Mux::try_get() else {
                    return;
                };
                let mut new_texts = HashMap::new();
                let snapshot = capture(&mux, &mut cache, &mut new_texts);
                let layout = match serde_json::to_string(&snapshot) {
                    Ok(layout) => layout,
                    Err(err) => {
                        log::error!("session: encoding snapshot: {err:#}");
                        continue;
                    }
                };
                if layout == last_layout {
                    continue;
                }
                let refs: BTreeSet<String> = snapshot
                    .panes
                    .values()
                    .filter_map(|p| p.text.clone())
                    .collect();
                match store.insert(&layout, &new_texts, &refs) {
                    Ok(()) => last_layout = layout,
                    Err(err) => {
                        log::error!("session: saving snapshot: {err:#}");
                        // Cached hashes may name text that never committed.
                        cache.clear();
                    }
                }
            }
        })
        .expect("spawning session-saver thread");
}

/// Blocks until a change arrives, then until changes stop for `SAVE_QUIET` or
/// `SAVE_MAX_DELAY` passes. False once the mux is gone.
fn settled(woken: &Receiver<()>) -> bool {
    if woken.recv().is_err() {
        return false;
    }
    let deadline = Instant::now() + SAVE_MAX_DELAY;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        match woken.recv_timeout(SAVE_QUIET.min(deadline - now)) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn snapshot(text: &str) -> Snapshot {
        Snapshot {
            windows: vec![WindowState {
                workspace: "work".into(),
                title: "win".into(),
                active_tab: 0,
                tabs: vec![TabState {
                    title: "tab".into(),
                    size: TerminalSize::default(),
                    tree: PaneNode::Empty,
                }],
            }],
            panes: HashMap::from([(
                7,
                PaneState {
                    domain: "local".into(),
                    text: Some(text.into()),
                    relaunch: Relaunch::Run("pi --session abc".into()),
                },
            )]),
        }
    }

    fn save(store: &mut Store, hash: &str, raw: &[u8]) {
        let layout = serde_json::to_string(&snapshot(hash)).unwrap();
        let texts = HashMap::from([(hash.to_string(), raw.to_vec())]);
        store
            .insert(&layout, &texts, &BTreeSet::from([hash.to_string()]))
            .unwrap();
    }

    fn count(store: &Store, table: &str) -> i64 {
        store
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn newest_returns_saved_layout_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("s.db")).unwrap();
        assert!(store.newest().unwrap().is_none());

        save(&mut store, "h1", b"\x1b[31mred\x1b[0m\r\n");
        save(&mut store, "h2", b"second\r\n");

        let (snap, texts) = store.newest().unwrap().unwrap();
        assert_eq!(snap, snapshot("h2"));
        assert_eq!(
            texts,
            HashMap::from([("h2".to_string(), b"second\r\n".to_vec())])
        );
    }

    #[test]
    fn keeps_newest_snapshots_and_drops_unreferenced_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("s.db")).unwrap();
        for i in 0..KEEP_SNAPSHOTS + 5 {
            save(&mut store, &format!("h{i}"), b"x");
        }
        assert_eq!(count(&store, "snapshot"), KEEP_SNAPSHOTS);
        assert_eq!(count(&store, "text"), KEEP_SNAPSHOTS);
    }

    #[test]
    fn reopened_store_sees_committed_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        save(&mut Store::open(&path).unwrap(), "h1", b"kept\r\n");

        let (snap, _) = Store::open(&path).unwrap().newest().unwrap().unwrap();
        assert_eq!(snap, snapshot("h1"));
    }

    #[test]
    fn second_owner_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let _first = Store::open(&path).unwrap();
        let err = Store::open(&path).err().unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("owned by another mux server"), "{}", msg);
    }
}
