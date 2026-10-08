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
//! EVENT-DRIVEN (owner 2026-10-08: no idle loops): the lib's `watch_path` on
//! [`BRIDGE`] — a read-only file_bridge rooted at the project, exposing the
//! bundle folders (host `.fantastic/bundles`, the pages' `.fantastic/web`) —
//! pushes `changed {path, kind}` from the disk's own events (FSEvents /
//! inotify). It is not recursive: the bundles dir is watched (a folder added or
//! removed) and so is each bundle's own folder. A burst (an editor's save) is
//! taken together: after the first event, what else arrives in [`SETTLE`].
//! Nothing runs while nothing changes. The pages watch the same bridge for
//! their own `bundles/`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::kern::host::Host;

/// The bridge whose `watch_path` reports bundle-folder changes.
pub const BRIDGE: &str = "bundles_watch";
/// The host's bundle folders, under the bridge's root (the project).
const BUNDLES: &str = ".fantastic/bundles";
/// A burst of events (one save) is taken together within this.
const SETTLE: Duration = Duration::from_millis(150);
/// The watcher's own inbox (the bridge's events land here).
const INBOX: &str = "bundle_watch";

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

fn err_of(r: &Value) -> Option<String> {
    r.get("error").and_then(Value::as_str).map(str::to_string)
}

/// The bridge, made when missing: read-only, rooted at the project, exposing
/// the host's and the pages' bundle folders, open (the pages watch it too).
pub async fn ensure_bridge(h: &Host) -> Result<(), String> {
    if h.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(BRIDGE)) {
        return Ok(());
    }
    let r = h
        .operator("kernel", json!({"type": "create_agent", "parent_id": crate::env::HUMAN, "id": BRIDGE,
            "handler_module": "file_bridge.tools", "root": ".", "readonly": true,
            "allow": [BUNDLES, ".fantastic/web"], "ingress_rule": "allow_all_ingress"}))
        .await
        .result;
    err_of(&r).map_or(Ok(()), |e| Err(format!("the bundle watcher's bridge `{BRIDGE}`: {e}")))
}

async fn watch_path(h: &Host, path: &str) {
    let r = h.operator(BRIDGE, json!({"type": "watch_path", "path": path})).await.result;
    if let Some(e) = err_of(&r) {
        tracing::warn!("bundle watch {path}: {e}");
    }
}

/// The bundle a changed path belongs to, and whether it is the bundle's folder
/// itself (added / removed) rather than a file in it.
fn bundle_of(path: &str) -> Option<(String, bool)> {
    let rest = path.trim_start_matches("./").strip_prefix(BUNDLES)?.trim_start_matches('/');
    let mut parts = rest.split('/');
    let name = parts.next().filter(|n| !n.is_empty())?.to_string();
    Some((name, parts.next().is_none()))
}

/// Watch `<store>/bundles/` for the life of the process — on the disk's events.
pub fn spawn(h: Arc<Host>, store: PathBuf) {
    let root = store.join("bundles");
    tokio::spawn(async move {
        if let Err(e) = std::fs::create_dir_all(&root) {
            tracing::warn!("{}: {e}", root.display());
            return;
        }
        if let Err(e) = ensure_bridge(&h).await {
            tracing::warn!("{e}");
            return;
        }
        let inbox = fantastic_kernel::AgentId::from(INBOX);
        let mut rx = h.kernel.ensure_inbox(&inbox);
        if let Some(denial) = h.kernel.watch(&fantastic_kernel::AgentId::from(BRIDGE), inbox.clone()).await {
            tracing::warn!("bundle watch: {denial}");
            return;
        }
        watch_path(&h, BUNDLES).await;
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                if let Some(n) = e.file_name().to_str() {
                    watch_path(&h, &format!("{BUNDLES}/{n}")).await;
                }
            }
        }
        while let Some(first) = rx.recv().await {
            // the burst: this event and every one within SETTLE
            let mut items = vec![first];
            tokio::time::sleep(SETTLE).await;
            while let Some(more) = rx.try_recv() {
                items.push(more);
            }
            let mut code: BTreeSet<String> = BTreeSet::new();
            let mut views: BTreeSet<String> = BTreeSet::new();
            for it in &items {
                let p = &it.payload;
                if p.get("type").and_then(Value::as_str) != Some("changed") {
                    continue;
                }
                let Some((name, is_folder)) = p.get("path").and_then(Value::as_str).and_then(bundle_of) else { continue };
                let dir = root.join(&name);
                if !dir.is_dir() {
                    continue; // removed: nothing to load
                }
                if is_folder {
                    // a NEW bundle: watch its files too, and load it
                    watch_path(&h, &format!("{BUNDLES}/{name}")).await;
                    code.insert(name);
                    continue;
                }
                let file = p.get("path").and_then(Value::as_str).unwrap_or("");
                let is_view = view_file(&dir).is_some_and(|v| v.file_name() == Path::new(file).file_name());
                if is_view { views.insert(name) } else { code.insert(name) };
            }
            for name in &code {
                let r = h.operator("kernel", json!({"type": "reload_bundle", "name": name})).await.result;
                if let Some(e) = err_of(&r) {
                    tracing::warn!("bundle {name}: reload: {e}");
                }
            }
            for name in views.difference(&code) {
                if let Err(e) = refresh_views(&h, name, &root.join(name)).await {
                    tracing::warn!("bundle {name}: {e}");
                }
            }
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
