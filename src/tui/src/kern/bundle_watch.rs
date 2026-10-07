//! THE DEV WATCHER (owner, 2026-10-08): a change under a host bundle's folder
//! (`.fantastic/bundles/<name>/`, `tmp/folder_bundles.md`) takes effect AT ONCE.
//!
//! - any file of the bundle but its view → the kernel's `reload_bundle {name}`:
//!   `bundle.yaml` re-read and every live instance stopped ABRUPTLY and restarted
//!   (in-flight calls settle `reloaded`) — the owner's rule for a code change;
//! - its VIEW file (the `record.view.bytes_file` its `bundle.yaml` names) → the
//!   instances' `view.bytes` rewritten from it, so their cells re-render — no
//!   restart for a view change.
//!
//! A poll of the folders' file times (every [`TICK`]): no watcher dependency, and
//! a folder that appears is a new bundle (`reload_bundle` loads it).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::kern::host::Host;

const TICK: Duration = Duration::from_millis(500);

/// (path → (mtime, len)) of every file in one bundle folder.
type Sig = BTreeMap<PathBuf, (SystemTime, u64)>;

fn sig_of(dir: &Path) -> Sig {
    let mut out = Sig::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                // a program's own caches are not its code
                if p.file_name().is_some_and(|n| n == "__pycache__" || n == ".git") {
                    continue;
                }
                stack.push(p);
            } else if let Ok(t) = m.modified() {
                out.insert(p, (t, m.len()));
            }
        }
    }
    out
}

/// The view a bundle declares for its instances (`record.view` in `bundle.yaml`).
fn bundle_view(dir: &Path) -> Option<serde_json::Map<String, Value>> {
    let text = std::fs::read_to_string(dir.join("bundle.yaml")).ok()?;
    let doc: Value = serde_yaml::from_str(&text).ok()?;
    doc.get("record")?.get("view")?.as_object().cloned()
}

/// The view file a bundle names (`record.view.bytes_file`, relative to its folder).
pub fn view_file(dir: &Path) -> Option<PathBuf> {
    let f = bundle_view(dir)?.get("bytes_file")?.as_str()?.to_string();
    Some(dir.join(f))
}

/// Rewrite every instance of bundle `name`'s `view.bytes` from its view file.
pub async fn refresh_views(h: &Host, name: &str, dir: &Path) -> Result<(), String> {
    let Some(f) = view_file(dir) else { return Ok(()) };
    let body = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
    let instances: Vec<String> = h
        .kernel
        .agents
        .iter()
        .filter(|a| a.value().record().handler_module.as_deref() == Some(name))
        .map(|a| a.key().0.clone())
        .collect();
    for id in instances {
        // the BUNDLE's view (kind, calls) — a default the kernel applies at run
        // time, not on the record — under the instance's own
        let mut view = bundle_view(dir).unwrap_or_default();
        if let Some(own) = h
            .kernel
            .agents
            .get(&fantastic_kernel::AgentId::from(id.as_str()))
            .and_then(|a| a.value().record().meta.get("view").and_then(Value::as_object).cloned())
        {
            view.extend(own);
        }
        // the view's `{{agent}}` is the instance it shows (one bundle, many instances)
        view.insert("bytes".into(), json!(body.replace("{{agent}}", &id)));
        let r = h.operator("kernel", json!({"type": "update_agent", "id": id, "view": view})).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(format!("{id}: its view: {e}"));
        }
    }
    Ok(())
}

/// Watch `<store>/bundles/` for the life of the process.
pub fn spawn(h: Arc<Host>, store: PathBuf) {
    let root = store.join("bundles");
    tokio::spawn(async move {
        let mut seen: BTreeMap<String, Sig> = BTreeMap::new();
        let mut first = true;
        loop {
            let mut now: BTreeMap<String, Sig> = BTreeMap::new();
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        if let Some(n) = e.file_name().to_str() {
                            now.insert(n.to_string(), sig_of(&e.path()));
                        }
                    }
                }
            }
            if !first {
                for (name, sig) in &now {
                    let was = seen.get(name);
                    if was == Some(sig) {
                        continue;
                    }
                    let dir = root.join(name);
                    let view = view_file(&dir);
                    // which files changed (all of them, for a new folder)
                    let changed: Vec<&PathBuf> = sig.iter().filter(|(p, v)| was.and_then(|w| w.get(*p)) != Some(v)).map(|(p, _)| p).collect();
                    let code = was.is_none() || changed.iter().any(|p| Some(*p) != view.as_ref());
                    if code {
                        let r = h.operator("kernel", json!({"type": "reload_bundle", "name": name})).await.result;
                        if let Some(e) = r.get("error").and_then(Value::as_str) {
                            tracing::warn!("bundle {name}: reload: {e}");
                        }
                    }
                    if view.as_ref().is_some_and(|v| changed.contains(&v)) {
                        if let Err(e) = refresh_views(&h, name, &dir).await {
                            tracing::warn!("bundle {name}: {e}");
                        }
                    }
                }
            }
            first = false;
            seen = now;
            tokio::time::sleep(TICK).await;
        }
    });
}

/// The starter bundles the CLI ships (the double-click's "python agent"): files
/// copied into a NEW project bundle folder, the lib's reference helper beside them.
pub const STARTERS: &[(&str, &[(&str, &str)])] = &[(
    "python_agent",
    &[
        ("bundle.yaml", include_str!("../../starters/python_agent/bundle.yaml")),
        ("agent.py", include_str!("../../starters/python_agent/agent.py")),
        ("panel.html", include_str!("../../starters/python_agent/panel.html")),
        ("fantastic_agent.py", include_str!("../../../lib/python/bundled_agents/io/py_agent/src/py_agent/fantastic_agent.py")),
    ],
)];

/// Copy starter `kind` into `<store>/bundles/<name>/` (a NEW folder — refused if
/// it exists). Returns the folder.
pub fn scaffold(store: &Path, kind: &str, name: &str) -> Result<PathBuf, String> {
    let files = STARTERS.iter().find(|(k, _)| *k == kind).map(|(_, f)| *f).ok_or_else(|| {
        format!("no starter `{kind}` — the starters are {}", STARTERS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", "))
    })?;
    let dir = store.join("bundles").join(name);
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (f, body) in files {
        std::fs::write(dir.join(f), body).map_err(|e| format!("{}/{f}: {e}", dir.display()))?;
    }
    Ok(dir)
}
