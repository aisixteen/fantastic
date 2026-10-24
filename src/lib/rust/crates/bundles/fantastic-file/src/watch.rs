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

use fantastic_kernel::{AgentId, Kernel};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};

/// A burst of disk events (an editor's save) inside this window is one `changed`
/// per path (python `WATCH_DEBOUNCE_MS`).
const DEBOUNCE: Duration = Duration::from_millis(50);

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
/// is an event, and a change before it is in the layout.
fn settle(
    root: &Path,
    norms: &[String],
    watcher: &Weak<Mutex<RecommendedWatcher>>,
    armed: &mut BTreeSet<PathBuf>,
) -> Layout {
    loop {
        let lay = layout(root, norms);
        let fresh: Vec<PathBuf> = lay
            .watch
            .iter()
            .filter(|d| !armed.contains(*d))
            .cloned()
            .collect();
        if fresh.is_empty() {
            return lay;
        }
        if let Some(w) = watcher.upgrade() {
            if let Ok(mut w) = w.lock() {
                for d in &fresh {
                    // a directory that vanished meanwhile is simply not armed
                    let _ = w.watch(d, RecursiveMode::NonRecursive);
                }
            }
        }
        armed.extend(fresh);
    }
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
        held.paths.entry(norm.clone()).or_default().insert(caller());
        // A missing path is accepted and pending: its nearest existing ancestor
        // is armed until the path appears.
        let missing = rewatch(agent_id, held, root, kernel);
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
        rewatch(agent_id, held, root, kernel);
        json!({"watching": false, "paths": paths_json(held)})
    }

    /// The bridge is going away: its watcher (and so its task) goes with it.
    pub(crate) fn drop_bridge(&self, agent_id: &AgentId) {
        self.by_bridge.lock().expect("watches").remove(&agent_id.0);
    }
}

/// (Re)start the ONE disk watcher a bridge runs, over the paths held now. The old
/// watcher is dropped first: its channel closes and its task ends. Returns the
/// held paths that are missing (pending) at arming time.
fn rewatch(
    agent_id: &AgentId,
    held: &mut Held,
    root: &Path,
    kernel: &Arc<Kernel>,
) -> BTreeSet<String> {
    held.watcher = None;
    if held.paths.is_empty() {
        return BTreeSet::new();
    }
    let norms: Vec<String> = held.paths.keys().cloned().collect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let Ok(watcher) = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev);
        }
    }) else {
        return BTreeSet::new();
    };
    let watcher = Arc::new(Mutex::new(watcher));
    let weak = Arc::downgrade(&watcher);
    let mut armed = BTreeSet::new();
    let first = settle(root, &norms, &weak, &mut armed);
    let missing = first.missing.clone();
    // a held path that exists now is already present: its create-like events
    // (an overwrite may read as one) are `modified`, not a new `added`
    let existing: BTreeSet<String> = norms
        .iter()
        .filter(|n| !missing.contains(*n))
        .cloned()
        .collect();
    let mut waiting = first.missing;
    held.watcher = Some(watcher);
    let me = agent_id.clone();
    let k = Arc::clone(kernel);
    let root = root.to_path_buf();
    kernel.spawn_task(agent_id, "watch_path", async move {
        // paths already present in this watch (announced, or there at arming)
        let mut present = existing;
        let mut batch: Vec<notify::Event> = Vec::new();
        // the first pass has no events: it checks the disk as armed
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
            // re-arm closer to any held path whose folders have appeared since
            let lay = settle(&root, &norms, &weak, &mut armed);
            let mut seen: BTreeMap<String, &'static str> = BTreeMap::new();
            for ev in &batch {
                for p in &ev.paths {
                    if let Some(rel) = relative(p, &lay.files, &lay.dirs) {
                        if let Some(kind) = kind_of(&ev.kind, p) {
                            // a creation inside the batch wins over a trailing modify
                            let e = seen.entry(rel).or_insert(kind);
                            if kind == "added" {
                                *e = "added";
                            }
                        }
                    }
                }
            }
            batch.clear();
            // a waiting path that exists now has appeared, whether or not its
            // create event was delivered; a present one is already announced
            for rel in &waiting {
                if !present.contains(rel) && under(&root, rel).exists() {
                    seen.insert(rel.clone(), "added");
                }
            }
            // `allow` is read LIVE: a path the bridge stopped exposing after the
            // watch was armed reports nothing more.
            let allow = k
                .agents
                .get(&me)
                .map(|a| crate::allow_list(&a.record().meta))
                .unwrap_or_default();
            for (rel, kind) in seen {
                // a path already announced as present reads `modified` again, not `added`
                let kind = match kind {
                    "added" if present.contains(&rel) => "modified",
                    "added" => {
                        present.insert(rel.clone());
                        "added"
                    }
                    "deleted" => {
                        present.remove(&rel);
                        waiting.insert(rel.clone());
                        "deleted"
                    }
                    other => other,
                };
                if kind == "added" {
                    waiting.remove(&rel);
                }
                if crate::exposed(&allow, &rel) {
                    k.emit(&me, json!({"type": "changed", "path": rel, "kind": kind}))
                        .await;
                }
            }
            if closed {
                return;
            }
        }
    });
    missing
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

/// python `watchfiles.Change`'s names: `added`, `modified`, `deleted`. A rename
/// reads as whichever the path now is; a mere access is no change.
fn kind_of(kind: &EventKind, p: &Path) -> Option<&'static str> {
    match kind {
        EventKind::Access(_) => None,
        EventKind::Create(_) => Some("added"),
        EventKind::Remove(_) => Some("deleted"),
        EventKind::Modify(ModifyKind::Name(_)) => {
            Some(if p.exists() { "added" } else { "deleted" })
        }
        _ => Some(if p.exists() { "modified" } else { "deleted" }),
    }
}
