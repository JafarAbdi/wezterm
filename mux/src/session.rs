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
use wezterm_term::{SemanticType, TerminalSize};

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
        match relaunch {
            Relaunch::Run(cmd) => type_at_prompt(pane.pane_id(), format!("{cmd}\r")),
            Relaunch::Type(cmd) => type_at_prompt(pane.pane_id(), cmd.clone()),
            Relaunch::Shell => {}
        }
        mux.add_pane(pane)?;
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

/// Types `input` into the pane once its shell draws its first prompt, as a
/// person would. Written any earlier, it waits in the pty's line discipline,
/// which echoes it a second time, and the shell takes it while still starting
/// up, racing its own terminal queries and job control setup. The prompt is
/// recognised by its OSC 133 marks, so a shell that emits none gets nothing
/// typed. The restored scrollback carries no marks, so it cannot trigger this.
/// Call before `add_pane` starts the pty reader, so no output is missed.
fn type_at_prompt(pane_id: PaneId, input: String) {
    Mux::get().subscribe(move |n| match n {
        MuxNotification::PaneOutput(id) if id == pane_id => {
            let Some(pane) = Mux::get().get_pane(pane_id) else {
                return false;
            };
            let at_prompt = pane.get_semantic_zones().map_or(false, |zones| {
                zones.iter().any(|z| z.semantic_type == SemanticType::Prompt)
            });
            if !at_prompt {
                return true;
            }
            if let Err(err) = pane.writer().write_all(input.as_bytes()) {
                log::error!("session: typing into pane {pane_id}: {err:#}");
            }
            false
        }
        MuxNotification::PaneRemoved(id) => id != pane_id,
        _ => true,
    });
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
    // A program that retitles itself (node's process.title) overwrites its
    // argv in place and pads it with NULs, which read back as trailing empty
    // arguments; `ps` drops them the same way.
    let mut argv = fg.argv;
    while argv.last().is_some_and(|arg| arg.is_empty()) {
        argv.pop();
    }
    Relaunch::Type(shell_words::join(&argv))
}

/// Captures on the main thread, which owns the mux. `Tab::codec_pane_tree`
/// locks the tab and then the mux windows, while `Mux::prune_dead_windows`
/// on the main thread locks them the other way round, so capturing from the
/// saver thread deadlocks against it. None once the mux is gone.
fn capture_on_main_thread(
    mut cache: HashMap<PaneId, CapturedText>,
) -> Option<(
    Snapshot,
    HashMap<String, Vec<u8>>,
    HashMap<PaneId, CapturedText>,
)> {
    let task = promise::spawn::spawn_into_main_thread(async move {
        let mut new_texts = HashMap::new();
        let snapshot = Mux::try_get().map(|mux| capture(&mux, &mut cache, &mut new_texts));
        (snapshot, new_texts, cache)
    });
    let (snapshot, new_texts, cache) = promise::spawn::block_on(task.fallible())?;
    Some((snapshot?, new_texts, cache))
}

/// The saver thread's state between captures.
struct Saver {
    store: Store,
    cache: HashMap<PaneId, CapturedText>,
    /// The snapshot last saved, so an unchanged mux is not saved again.
    last: Option<Snapshot>,
}

impl Saver {
    fn new(store: Store) -> Self {
        Self {
            store,
            cache: HashMap::new(),
            last: None,
        }
    }

    fn save(&mut self, snapshot: Snapshot, new_texts: &HashMap<String, Vec<u8>>) {
        if self.last.as_ref() == Some(&snapshot) {
            return;
        }
        let layout = match serde_json::to_string(&snapshot) {
            Ok(layout) => layout,
            Err(err) => {
                log::error!("session: encoding snapshot: {err:#}");
                return;
            }
        };
        let refs: BTreeSet<String> = snapshot
            .panes
            .values()
            .filter_map(|p| p.text.clone())
            .collect();
        match self.store.insert(&layout, new_texts, &refs) {
            Ok(()) => self.last = Some(snapshot),
            Err(err) => {
                log::error!("session: saving snapshot: {err:#}");
                // Cached hashes may name text that never committed.
                self.cache.clear();
            }
        }
    }
}

/// Saves the mux into `store` after changes settle, on a dedicated thread.
pub fn start_saver(store: Store) {
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
            let mut saver = Saver::new(store);
            while settled(&woken) {
                let cache = std::mem::take(&mut saver.cache);
                let Some((snapshot, new_texts, cache)) = capture_on_main_thread(cache) else {
                    return;
                };
                saver.cache = cache;
                saver.save(snapshot, &new_texts);
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
    use crate::domain::Domain;
    use crate::tab::test::FakePane;
    use std::thread;

    const SIZE: TerminalSize = TerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 800,
        pixel_height: 600,
        dpi: 96,
    };

    /// The tasks `promise::spawn::spawn_into_main_thread` queues from now on.
    fn main_thread_tasks() -> Receiver<promise::spawn::Runnable> {
        let (tx, rx) = crossbeam::channel::unbounded();
        let low = tx.clone();
        promise::spawn::set_schedulers(
            Box::new(move |task| {
                tx.send(task).ok();
            }),
            Box::new(move |task| {
                low.send(task).ok();
            }),
        );
        rx
    }

    /// A window holding one tab with one local pane, and what `capture`
    /// must produce for it.
    fn window_with_one_local_pane(mux: &Mux) -> Snapshot {
        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local").unwrap());
        mux.add_domain(&domain);
        let pane = FakePane::in_domain(70, SIZE, domain.domain_id());
        mux.add_pane(&pane).unwrap();
        let tab = Arc::new(Tab::new(&SIZE));
        tab.assign_pane(&pane);
        mux.add_tab_no_panes(&tab);
        let window_id = *mux.new_empty_window(None, None);
        mux.add_tab_to_window(&tab, window_id).unwrap();

        Snapshot {
            windows: vec![WindowState {
                workspace: "default".into(),
                title: String::new(),
                active_tab: 0,
                tabs: vec![TabState {
                    title: String::new(),
                    size: SIZE,
                    tree: PaneNode::Leaf(PaneEntry {
                        window_id,
                        tab_id: tab.tab_id(),
                        pane_id: 70,
                        title: "pane 70".into(),
                        is_active_pane: true,
                        is_zoomed_pane: false,
                        size: SIZE,
                        working_dir: None,
                        workspace: "default".into(),
                        cursor_pos: Default::default(),
                        physical_top: 0,
                        left_col: 0,
                        top_row: 0,
                        tty_name: None,
                    }),
                }],
            }],
            panes: HashMap::from([(
                70,
                PaneState {
                    domain: "local".into(),
                    text: None,
                    relaunch: Relaunch::Shell,
                },
            )]),
        }
    }

    /// The saver captures while the main thread prunes dead windows.
    /// `Tab::codec_pane_tree` locks the tab and then the mux windows;
    /// `Mux::prune_dead_windows` locks the windows and then each tab.
    #[test]
    fn saver_captures_while_the_main_thread_prunes() {
        let tasks = main_thread_tasks();
        let mux = crate::test_mux();
        let expected = window_with_one_local_pane(&mux);
        let (done, finished) = bounded(1);

        let saver = thread::spawn(move || {
            let mut cache = HashMap::new();
            for _ in 0..10_000 {
                let (snapshot, _, returned) = capture_on_main_thread(cache).unwrap();
                assert_eq!(snapshot, expected);
                cache = returned;
            }
            done.send(()).unwrap();
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let main = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    mux.prune_dead_windows();
                    for task in tasks.try_iter() {
                        task.run();
                    }
                }
            })
        };

        let outcome = finished.recv_timeout(Duration::from_secs(10));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            outcome.is_ok(),
            "capture and prune_dead_windows deadlocked for 10s"
        );
        saver.join().unwrap();
        main.join().unwrap();
    }

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

    /// A snapshot of panes `ids`. Two calls build their pane maps with
    /// different hashers, so equal snapshots need not encode to equal JSON.
    fn snapshot_of_panes(ids: impl Iterator<Item = PaneId>) -> Snapshot {
        Snapshot {
            windows: vec![],
            panes: ids
                .map(|id| {
                    (
                        id,
                        PaneState {
                            domain: "local".into(),
                            text: None,
                            relaunch: Relaunch::Shell,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn an_unchanged_snapshot_is_not_saved_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut saver = Saver::new(Store::open(&dir.path().join("s.db")).unwrap());

        saver.save(snapshot_of_panes(0..16), &HashMap::new());
        saver.save(snapshot_of_panes(0..16), &HashMap::new());

        assert_eq!(count(&saver.store, "snapshot"), 1);
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
