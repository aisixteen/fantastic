//! Watching paths — python `file_bridge._watch_path`, text for
//! text.
//!
//! `watch_path {path}` puts an EXPOSED file or directory under a disk watcher
//! (`notify` — native events: FSEvents / inotify / kqueue): any change to it, by
//! any writer (this bridge, an editor, another process), is emitted on this
//! bridge's own inbox as `{type: changed, path, kind}`. A caller receives them by
//! WATCHING this bridge. A file is watched through its parent directory, so an
//! editor's atomic save (a temp file renamed over) is seen. Each path is held per
//! caller; the disk watch stops when no caller holds any path. RAM only.
//!
//! A path that does not exist yet is held as pending. Its nearest existing
//! folder is watched (non-recursively) and the watch moves down the tree as each
//! folder appears. After every batch of events, and after every re-arm, the
//! pending paths are checked on disk, so an `added` is reported even when the
//! OS did not deliver the create event (inotify does not replay the events of a
//! folder that is created before its watch is in place).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use fantastic_bundle::py_repr;
use fantastic_kernel::{refusal as rf, AgentId, Kernel};
use notify::event::EventKind;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};

use crate::views;

/// A burst of disk events (an editor's save) inside this window is one `changed`
/// per path (python `WATCH_DEBOUNCE_MS`).
const DEBOUNCE: Duration = Duration::from_millis(50);

/// What a path looked like on disk when it was last judged: enough to tell the
/// same file changed (size, times) from a different file at the same path (a
/// new inode: an atomic save, a restore, a copy over). The platform's event
/// names are not used for this, because a replayed event can carry a stale name.
#[derive(Debug, Clone, PartialEq)]
struct Stamp {
    ino: u64,
    ctime_ns: i128,
    mtime_ns: i128,
    size: u64,
}

/// The stamp of the path now, or `None` when nothing is there.
#[cfg(unix)]
fn stamp_of(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).ok()?;
    Some(Stamp {
        ino: md.ino(),
        ctime_ns: i128::from(md.ctime()) * 1_000_000_000 + i128::from(md.ctime_nsec()),
        mtime_ns: i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec()),
        size: md.size(),
    })
}

/// The stamp of the path now. The identity is the volume's file index, read from
/// an open handle (a directory is opened with backup semantics so it can be read
/// too). The times are the last write time: the handle API does not give a
/// change time here, so a metadata-only change on Windows is not seen.
#[cfg(windows)]
fn stamp_of(path: &Path) -> Option<Stamp> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let md = std::fs::metadata(path).ok()?;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid out-parameter,
    // and the handle is open for the duration of the call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `f` owns a live handle; `info` is a valid, writable struct.
    if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
        return None;
    }
    let ino = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    // FILETIME: 100 ns ticks; the same value stands for both times here.
    let written = i128::from(md.last_write_time()) * 100;
    Some(Stamp {
        ino,
        ctime_ns: written,
        mtime_ns: written,
        size: md.file_size(),
    })
}

/// The judgement of one path between its last stamp and its stamp now (python
/// `watchfiles.Change` names). A different inode is `added` (a new file at the
/// path); the same inode with any other field changed is `modified`; the same
/// stamp, or no file either time, is no change.
fn judge(last: Option<&Stamp>, now: Option<&Stamp>) -> Option<&'static str> {
    match (last, now) {
        (None, None) => None,
        (None, Some(_)) => Some("added"),
        (Some(_), None) => Some("deleted"),
        (Some(a), Some(b)) if a.ino != b.ino => Some("added"),
        (Some(a), Some(b)) if a == b => None,
        (Some(_), Some(_)) => Some("modified"),
    }
}

/// What one bridge holds: each watched path and who holds it, and the live
/// watcher (dropping it ends its task: the task keeps only a `Weak` to it).
#[derive(Default)]
struct Held {
    paths: BTreeMap<String, BTreeSet<String>>,
    watcher: Option<Arc<Mutex<RecommendedWatcher>>>,
}

/// Every bridge's watches, for the kernel the bundle is registered in.
#[derive(Default)]
pub(crate) struct Watches {
    by_bridge: Mutex<HashMap<String, Held>>,
}

/// Who holds a watch: the sending agent, or the operator.
fn caller() -> String {
    fantastic_kernel::send::current_origin().map_or_else(|| "kernel".to_string(), |a| a.0)
}

fn parent_of(norm: &str) -> String {
    match norm.rsplit_once('/') {
        Some((p, _)) if !p.is_empty() => p.to_string(),
        _ => ".".to_string(),
    }
}

fn under(root: &Path, norm: &str) -> PathBuf {
    if norm == "." {
        root.to_path_buf()
    } else {
        root.join(norm)
    }
}

/// The nearest existing folder above a missing target, and the missing tail
/// below it (the part a later `added` event will name).
fn nearest_existing(target: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut anc = target.parent()?.to_path_buf();
    loop {
        if anc.is_dir() {
            let rest = target.strip_prefix(&anc).ok()?.to_path_buf();
            return Some((anc, rest));
        }
        anc = anc.parent()?.to_path_buf();
    }
}

fn paths_json(held: &Held) -> Value {
    json!(held.paths.keys().collect::<Vec<_>>())
}

/// What the held paths look like on disk right now.
#[derive(Default)]
struct Layout {
    /// canonical file path → the watched path it reports as
    files: HashMap<PathBuf, String>,
    /// canonical directory → the watched path it reports as (its entries map
    /// to `<path>/<name>`)
    dirs: HashMap<PathBuf, String>,
    /// the held paths whose target does not exist
    missing: BTreeSet<String>,
    /// the directories that must be watched (non-recursively) for every change
    /// to a held path to be an event
    watch: BTreeSet<PathBuf>,
}

fn layout(root: &Path, norms: &[String]) -> Layout {
    let mut l = Layout::default();
    for norm in norms {
        let target = under(root, norm);
        if target.is_dir() {
            if let Ok(c) = target.canonicalize() {
                l.watch.insert(c.clone());
                l.dirs.insert(c, norm.clone());
            }
        } else if target.is_file() {
            if let (Some(parent), Some(name)) = (target.parent(), target.file_name()) {
                if let Ok(c) = parent.canonicalize() {
                    l.watch.insert(c.clone());
                    l.files.insert(c.join(name), norm.clone());
                }
            }
        } else if !target.exists() {
            l.missing.insert(norm.clone());
            if let Some((anc, rest)) = nearest_existing(&target) {
                if let Ok(c) = anc.canonicalize() {
                    l.watch.insert(c.clone());
                    l.files.insert(c.join(&rest), norm.clone());
                }
            }
        }
    }
    l
}

/// Watch every directory the held paths need, then describe the disk again.
/// Repeats until the description needs nothing new, so the returned layout was
/// computed after every directory it names was armed: a change after that point
/// is an event, and a change before it is in the layout. A directory that cannot
/// be armed while it still exists is an error (the first one, as text); a
/// directory that vanished meanwhile is not.
fn settle(
    root: &Path,
    norms: &[String],
    watcher: &Weak<Mutex<RecommendedWatcher>>,
    armed: &mut BTreeSet<PathBuf>,
) -> Result<Layout, String> {
    loop {
        let lay = layout(root, norms);
        let fresh: Vec<PathBuf> = lay
            .watch
            .iter()
            .filter(|d| !armed.contains(*d))
            .cloned()
            .collect();
        if fresh.is_empty() {
            return Ok(lay);
        }
        if let Some(w) = watcher.upgrade() {
            if let Ok(mut w) = w.lock() {
                for d in &fresh {
                    if let Err(e) = w.watch(d, RecursiveMode::NonRecursive) {
                        if d.exists() {
                            return Err(format!("{}: {e}", d.display()));
                        }
                    }
                }
            }
        }
        armed.extend(fresh);
    }
}

/// The stamp of every held path that exists now, as the seed of the judgement:
/// a held file, and every current entry of a held directory (one level, the watch
/// is not recursive). Taken right after the arm, so a later event is judged
/// against what the disk held when the watch went live.
fn seed(root: &Path, norms: &[String]) -> HashMap<String, Stamp> {
    let mut stamps = HashMap::new();
    for norm in norms {
        let target = under(root, norm);
        if let Some(s) = stamp_of(&target) {
            stamps.insert(norm.clone(), s);
        }
        if target.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&target) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let rel = if norm == "." {
                        name
                    } else {
                        format!("{norm}/{name}")
                    };
                    if let Some(s) = stamp_of(&under(root, &rel)) {
                        stamps.insert(rel, s);
                    }
                }
            }
        }
    }
    stamps
}

impl Watches {
    /// `watch_path {path}` after the exposure gate (the caller's `exposure`).
    pub(crate) fn watch_path(
        &self,
        agent_id: &AgentId,
        root: &Path,
        path: &str,
        kernel: &Arc<Kernel>,
    ) -> Value {
        let norm = crate::norm(path)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| ".".into());
        let mut all = self.by_bridge.lock().expect("watches");
        let held = all.entry(agent_id.0.clone()).or_default();
        let me = caller();
        let was_holder = held.paths.get(&norm).is_some_and(|h| h.contains(&me));
        held.paths
            .entry(norm.clone())
            .or_default()
            .insert(me.clone());
        // A missing path is accepted and pending: its nearest existing ancestor
        // is armed until the path appears.
        let missing = match rewatch(agent_id, held, root, kernel) {
            Ok(missing) => missing,
            Err(e) => {
                // a watch that did not arm is not a watch: roll this caller's new
                // hold back, and re-arm what the others still hold
                if !was_holder {
                    if let Some(h) = held.paths.get_mut(&norm) {
                        h.remove(&me);
                        if h.is_empty() {
                            held.paths.remove(&norm);
                        }
                    }
                }
                let _ = rewatch(agent_id, held, root, kernel);
                return rf::refusal_hint(
                    rf::TRANSPORT_ERROR,
                    format!(
                        "watch_path {}: the disk watch cannot be armed: {e}",
                        py_repr(&norm)
                    ),
                    "check that the path and its folder are readable",
                );
            }
        };
        let mut out = json!({"watching": norm, "paths": paths_json(held)});
        if missing.contains(&norm) {
            out["pending"] = json!(true);
        }
        out
    }

    /// `unwatch_path {path}`: drop the caller's hold.
    pub(crate) fn unwatch_path(
        &self,
        agent_id: &AgentId,
        root: &Path,
        path: &str,
        kernel: &Arc<Kernel>,
    ) -> Value {
        let norm = crate::norm(path)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| ".".into());
        let mut all = self.by_bridge.lock().expect("watches");
        let held = all.entry(agent_id.0.clone()).or_default();
        if let Some(holders) = held.paths.get_mut(&norm) {
            holders.remove(&caller());
            if holders.is_empty() {
                held.paths.remove(&norm);
            }
        }
        // what is left was armed before; a re-arm that fails now leaves no watch
        // for it, and the reply still reports the hold as dropped
        let _ = rewatch(agent_id, held, root, kernel);
        json!({"watching": false, "paths": paths_json(held)})
    }

    /// The bridge is going away: its watcher (and so its task) goes with it.
    pub(crate) fn drop_bridge(&self, agent_id: &AgentId) {
        self.by_bridge.lock().expect("watches").remove(&agent_id.0);
    }
}

/// (Re)start the ONE disk watcher a bridge runs, over the paths held now. The old
/// watcher is dropped first: its channel closes and its task ends. Returns the
/// held paths that are missing (pending) at arming time, or why the watch could
/// not be armed.
fn rewatch(
    agent_id: &AgentId,
    held: &mut Held,
    root: &Path,
    kernel: &Arc<Kernel>,
) -> Result<BTreeSet<String>, String> {
    held.watcher = None;
    if held.paths.is_empty() {
        return Ok(BTreeSet::new());
    }
    let norms: Vec<String> = held.paths.keys().cloned().collect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev);
        }
    })
    .map_err(|e| format!("the disk watcher did not start: {e}"))?;
    let watcher = Arc::new(Mutex::new(watcher));
    let weak = Arc::downgrade(&watcher);
    let mut armed = BTreeSet::new();
    let mut lay = settle(root, &norms, &weak, &mut armed)?;
    let missing = lay.missing.clone();
    // the stamps the judgement starts from: what the disk holds at the arm
    let mut stamps = seed(root, &norms);
    held.watcher = Some(watcher);
    let me = agent_id.clone();
    let k = Arc::clone(kernel);
    // The holders as they stand now: every watch change re-arms this task, so the
    // snapshot is the holders of this watcher's life.
    let holders = held.paths.clone();
    let root = root.to_path_buf();
    kernel.spawn_task(agent_id, "watch_path", async move {
        let mut batch: Vec<notify::Event> = Vec::new();
        // the first pass has no events: it judges the pending paths as armed
        let mut first_pass = true;
        loop {
            let mut closed = false;
            if !first_pass {
                let Some(e) = rx.recv().await else { return };
                batch.push(e);
                loop {
                    match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                        Ok(Some(e)) => batch.push(e),
                        Ok(None) => {
                            closed = true;
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
            first_pass = false;
            // re-arm closer to any held path whose folders have appeared since; a
            // failed re-arm keeps the layout the watch already has
            if let Ok(l) = settle(&root, &norms, &weak, &mut armed) {
                lay = l;
            }
            // the paths to judge: the path of every event (an access is no
            // change), and every held path with no stamp (a pending one)
            let mut touched: BTreeSet<String> = BTreeSet::new();
            for ev in &batch {
                if matches!(ev.kind, EventKind::Access(_)) {
                    continue;
                }
                for p in &ev.paths {
                    if let Some(rel) = relative(p, &lay.files, &lay.dirs) {
                        touched.insert(rel);
                    }
                }
            }
            batch.clear();
            touched.extend(norms.iter().filter(|n| !stamps.contains_key(*n)).cloned());
            // each is judged against its LAST stamp, never the arm stamp, and the
            // stamp is kept current even when the path is not exposed
            let mut changes: Vec<(String, &'static str)> = Vec::new();
            for rel in touched {
                let now = stamp_of(&under(&root, &rel));
                if let Some(kind) = judge(stamps.get(&rel), now.as_ref()) {
                    changes.push((rel.clone(), kind));
                }
                match now {
                    Some(s) => {
                        stamps.insert(rel, s);
                    }
                    None => {
                        stamps.remove(&rel);
                    }
                }
            }
            // `allow` is read LIVE: a path the bridge stopped exposing after the
            // watch was armed reports nothing more.
            let allow = k
                .agents
                .get(&me)
                .map(|a| crate::allow_list(&a.record().meta))
                .unwrap_or_default();
            for (rel, kind) in changes {
                if crate::exposed(&allow, &rel) {
                    k.emit(&me, json!({"type": "changed", "path": rel, "kind": kind}))
                        .await;
                    relay_to_views(&k, &me, &holders, &rel, kind);
                }
            }
            if closed {
                return;
            }
        }
    });
    Ok(missing)
}

/// python `tools._relay_to_views`: push one change to each VIEW caller whose watch
/// covers it, and only where its scope on this bridge admits it (its `callable_by` entry
/// here). The kernel's own watch would mirror every change to a watcher, so a view is
/// never a kernel watcher of its bridge.
pub(crate) fn relay_to_views(
    kernel: &Arc<Kernel>,
    bridge: &AgentId,
    holders: &BTreeMap<String, BTreeSet<String>>,
    rel: &str,
    kind: &str,
) {
    let ev = json!({"type": "changed", "path": rel, "kind": kind});
    for (norm, callers) in holders {
        if rel != norm && parent_of(rel) != *norm {
            continue;
        }
        for caller in callers {
            let caller = AgentId::from(caller.as_str());
            if let Some((scope_kind, own)) = views::caller_scope(kernel, &bridge.0, Some(&caller)) {
                if views::inside(&scope_kind, &own, rel) {
                    views::deliver(kernel, &caller.0, ev.clone());
                }
            }
        }
    }
}

/// An event path → the watched path it belongs to (a file, or a watched
/// directory's entry), or `None`.
fn relative(
    p: &Path,
    files: &HashMap<PathBuf, String>,
    dirs: &HashMap<PathBuf, String>,
) -> Option<String> {
    let canon = match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => parent.canonicalize().ok()?.join(name),
        _ => return None,
    };
    if let Some(norm) = files.get(&canon) {
        return Some(norm.clone());
    }
    let d = dirs.get(canon.parent()?)?;
    let name = canon.file_name()?.to_string_lossy().to_string();
    Some(if d == "." {
        name
    } else {
        format!("{d}/{name}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(ino: u64, ctime_ns: i128, mtime_ns: i128, size: u64) -> Stamp {
        Stamp {
            ino,
            ctime_ns,
            mtime_ns,
            size,
        }
    }

    #[test]
    fn judge_covers_every_row_of_the_table() {
        let a = st(1, 10, 10, 4);
        assert_eq!(judge(None, Some(&a)), Some("added"));
        assert_eq!(judge(Some(&a), None), Some("deleted"));
        assert_eq!(judge(Some(&a), Some(&st(2, 10, 10, 4))), Some("added"));
        assert_eq!(judge(Some(&a), Some(&st(1, 10, 10, 5))), Some("modified"));
        assert_eq!(judge(Some(&a), Some(&st(1, 10, 11, 4))), Some("modified"));
        assert_eq!(judge(Some(&a), Some(&st(1, 11, 10, 4))), Some("modified"));
        assert_eq!(judge(Some(&a), Some(&a)), None);
        assert_eq!(judge(None, None), None);
    }

    #[test]
    fn judge_reports_delete_then_restore_with_a_preserved_stamp() {
        // the comparison is against the LAST stamp: a delete clears it, and the
        // same stamp back is a new file at the path
        let s = st(7, 100, 100, 3);
        let mut stamps: HashMap<&str, Stamp> = HashMap::new();
        stamps.insert("f", s.clone());
        assert_eq!(judge(stamps.get("f"), None), Some("deleted"));
        stamps.remove("f");
        assert_eq!(judge(stamps.get("f"), Some(&s)), Some("added"));
        stamps.insert("f", s.clone());
        assert_eq!(judge(stamps.get("f"), Some(&s)), None);
    }

    #[test]
    fn judge_chmod_only_is_modified_and_a_replay_is_dropped() {
        // same inode, mtime and size; only the change time moved (a chmod)
        let a = st(3, 100, 50, 9);
        assert_eq!(judge(Some(&a), Some(&st(3, 101, 50, 9))), Some("modified"));
        // a replayed event for an unchanged entry carries the same stamp
        assert_eq!(judge(Some(&a), Some(&a.clone())), None);
    }

    #[test]
    fn stamp_of_reads_a_file_and_nothing_at_a_missing_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("f");
        assert_eq!(stamp_of(&f), None);
        std::fs::write(&f, "abc").unwrap();
        let s = stamp_of(&f).expect("a stamp for an existing file");
        assert_eq!(s.size, 3);
        assert_eq!(stamp_of(&f), Some(s));
    }
}
