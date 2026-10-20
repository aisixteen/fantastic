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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fantastic_bundle::py_repr;
use fantastic_kernel::refusal as rf;
use fantastic_kernel::{AgentId, Kernel};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};

/// A burst of disk events (an editor's save) inside this window is one `changed`
/// per path (python `WATCH_DEBOUNCE_MS`).
const DEBOUNCE: Duration = Duration::from_millis(50);

/// What one bridge holds: each watched path and who holds it, and the live
/// watcher (dropping it ends its task).
#[derive(Default)]
struct Held {
    paths: BTreeMap<String, BTreeSet<String>>,
    watcher: Option<RecommendedWatcher>,
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

fn paths_json(held: &Held) -> Value {
    json!(held.paths.keys().collect::<Vec<_>>())
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
        let parent = parent_of(&norm);
        let is_dir = under(root, &norm).is_dir();
        if !(is_dir || under(root, &parent).is_dir()) {
            let mut r = rf::refusal(
                rf::NOT_FOUND,
                format!(
                    "watch_path: no directory {} to watch {} in",
                    py_repr(&parent),
                    py_repr(path)
                ),
            );
            rf::with(&mut r, "path", json!(path));
            return r;
        }
        let mut all = self.by_bridge.lock().expect("watches");
        let held = all.entry(agent_id.0.clone()).or_default();
        held.paths.entry(norm.clone()).or_default().insert(caller());
        rewatch(agent_id, held, root, kernel);
        json!({"watching": norm, "paths": paths_json(held)})
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
/// watcher is dropped first: its channel closes and its task ends.
fn rewatch(agent_id: &AgentId, held: &mut Held, root: &Path, kernel: &Arc<Kernel>) {
    held.watcher = None;
    if held.paths.is_empty() {
        return;
    }
    // canonical target → its path under the root
    let mut files: HashMap<PathBuf, String> = HashMap::new();
    let mut dirs: HashMap<PathBuf, String> = HashMap::new();
    for norm in held.paths.keys() {
        let target = under(root, norm);
        if target.is_dir() {
            if let Ok(c) = target.canonicalize() {
                dirs.insert(c, norm.clone());
            }
        } else if let (Some(parent), Some(name)) = (target.parent(), target.file_name()) {
            if let Ok(c) = parent.canonicalize() {
                files.insert(c.join(name), norm.clone());
            }
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let Ok(mut watcher) = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev);
        }
    }) else {
        return;
    };
    let targets: BTreeSet<PathBuf> = files
        .keys()
        .filter_map(|f| f.parent().map(Path::to_path_buf))
        .chain(dirs.keys().cloned())
        .collect();
    for t in &targets {
        let _ = watcher.watch(t, RecursiveMode::NonRecursive);
    }
    held.watcher = Some(watcher);
    let me = agent_id.clone();
    let k = Arc::clone(kernel);
    let mut known = snapshot(&files, &dirs);
    kernel.spawn_task(agent_id, "watch_path", async move {
        loop {
            let Some(first) = rx.recv().await else { return };
            let mut events = vec![first];
            let mut closed = false;
            loop {
                match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                    Ok(Some(e)) => events.push(e),
                    Ok(None) => {
                        closed = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            let mut seen: BTreeMap<String, &'static str> = BTreeMap::new();
            for ev in &events {
                for p in &ev.paths {
                    let (Some(rel), Some(key)) = (relative(p, &files, &dirs), canonical(p)) else {
                        continue;
                    };
                    let Some(kind) = kind_of(&ev.kind, p, known.contains(&key)) else {
                        continue;
                    };
                    if p.symlink_metadata().is_ok() {
                        known.insert(key);
                    } else {
                        known.remove(&key);
                    }
                    // a path created in this batch stays `added` whatever a later report says
                    let kind = match seen.get(&rel) {
                        Some(&"added") if kind == "modified" => "added",
                        _ => kind,
                    };
                    seen.insert(rel, kind);
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

/// python `watchfiles.Change`'s names: `added`, `modified`, `deleted`. A mere access
/// is no change. The DISK AS IT IS NOW decides `deleted` (python `_kind`): a batch
/// can carry one path twice (macOS FSEvents: `deleted` then a spurious `added` for
/// one unlink), and the last event must not read as the file's state.
fn kind_of(kind: &EventKind, p: &Path, known: bool) -> Option<&'static str> {
    if matches!(kind, EventKind::Access(_)) {
        return None;
    }
    if p.symlink_metadata().is_err() {
        return Some("deleted");
    }
    Some(match kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_)) => "added",
        // a platform may report a new file as a modify: the watch has not seen it before
        _ if !known => "added",
        _ => "modified",
    })
}

/// The entries the watch sees as it starts (canonical): each watched file that exists,
/// and each entry of a watched directory. A change to an entry not in here is `added`.
fn snapshot(
    files: &HashMap<PathBuf, String>,
    dirs: &HashMap<PathBuf, String>,
) -> BTreeSet<PathBuf> {
    let mut out: BTreeSet<PathBuf> = files
        .keys()
        .filter(|f| f.symlink_metadata().is_ok())
        .cloned()
        .collect();
    for d in dirs.keys() {
        if let Ok(entries) = std::fs::read_dir(d) {
            out.extend(entries.flatten().map(|e| d.join(e.file_name())));
        }
    }
    out
}

/// An event path as the disk names it: its parent canonical, its name kept.
fn canonical(p: &Path) -> Option<PathBuf> {
    let (parent, name) = (p.parent()?, p.file_name()?);
    Some(parent.canonicalize().ok()?.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, RemoveKind};

    #[test]
    fn a_change_is_judged_by_the_disk_as_it_is() {
        let dir = tempfile::TempDir::new().unwrap();
        let here = dir.path().join("here.md");
        std::fs::write(&here, "x").unwrap();
        let gone = dir.path().join("gone.md");
        // a spurious `added` after an unlink reads as deleted: the disk says so
        assert_eq!(
            kind_of(&EventKind::Create(CreateKind::File), &gone, false),
            Some("deleted")
        );
        assert_eq!(
            kind_of(&EventKind::Remove(RemoveKind::File), &gone, true),
            Some("deleted")
        );
        assert_eq!(
            kind_of(&EventKind::Create(CreateKind::File), &here, true),
            Some("added")
        );
        assert_eq!(
            kind_of(&EventKind::Modify(ModifyKind::Any), &here, true),
            Some("modified")
        );
        // present now, so a reported removal is not `deleted`
        assert_eq!(
            kind_of(&EventKind::Remove(RemoveKind::File), &here, true),
            Some("modified")
        );
        assert_eq!(
            kind_of(
                &EventKind::Access(notify::event::AccessKind::Any),
                &here,
                true
            ),
            None
        );
    }

    #[test]
    fn a_new_entry_is_added_even_when_the_platform_reports_a_modify() {
        let dir = tempfile::TempDir::new().unwrap();
        let fresh = dir.path().join("fresh.md");
        std::fs::write(&fresh, "x").unwrap();
        // the watch never saw it: added, whatever the platform named the event
        assert_eq!(
            kind_of(&EventKind::Modify(ModifyKind::Any), &fresh, false),
            Some("added")
        );
        assert_eq!(
            kind_of(&EventKind::Modify(ModifyKind::Any), &fresh, true),
            Some("modified")
        );
    }
}
