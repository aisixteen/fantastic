//! THE DEV WATCHER (owner, 2026-10-08): a change under a host bundle's folder
//! (`.fantastic/bundles/<name>/`, `tmp/folder_bundles.md`) takes effect AT ONCE.
//!
//! - any file of the bundle but its view → the kernel's `reload_bundle {name}`:
//!   `bundle.yaml` re-read and every live instance stopped ABRUPTLY and restarted
//!   (in-flight calls settle `reloaded`) — the owner's rule for a code change;
//! - its VIEW file (the `record.view.bytes_file` its `bundle.yaml` names) → the
//!   instances' `view` written again with the file's text as `view.bytes`
//!   (`{{agent}}` replaced by the instance id), so their cells re-render — no
//!   restart for a view change. The view reaches the canvas as `view.bytes`
//!   over the existing WS; the canvas reads no file.
//!
//! EVENT-DRIVEN (owner 2026-10-08: no idle loops): the lib's `watch_path` on the
//! root loader's DISCOVERED store (a file_bridge rooted at the state folder)
//! pushes `changed {path, kind}` from the disk's own events (FSEvents /
//! inotify). It is not recursive: `bundles` is watched (a folder added or
//! removed) and so is each bundle's own folder. A burst (an editor's save) is
//! taken together: after the first event, what else arrives in [`SETTLE`].
//! Nothing runs while nothing changes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::kern::host::Host;

/// The host's bundle folders, under the store's root (the state folder).
const BUNDLES: &str = "bundles";
/// A burst of events (one save) is taken together within this.
const SETTLE: Duration = Duration::from_millis(150);
/// The watcher's own inbox (the store's events land here).
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

/// Write every instance of bundle `name`'s `view` again: the bundle's declared
/// fields over the instance's own, and the view file's text as `bytes`
/// (`{{agent}}` replaced by the instance id). `bytes_rev` (the file's content
/// hash) makes the write a change; the change is the cells' signal to draw again.
pub async fn refresh_views(h: &Host, name: &str, dir: &Path) -> Result<(), String> {
    let Some(declared) = bundle_view(dir) else {
        return Ok(());
    };
    // the TUI part is checked BEFORE any instance is touched: a bad declaration
    // is refused with its sentence and every instance keeps the view it has
    if let Some(tui) = declared.get("tui") {
        crate::panel::declared::parse_tui(tui).map_err(|r| r.error)?;
    }
    // the panel's REVISION (only a view with a file has one): a view edit changes
    // no other field of the record, and an unchanged record redraws nothing
    let (rev, text) = match view_file(dir) {
        None => (None, None),
        Some(f) => {
            let bytes = std::fs::read(&f).map_err(|e| format!("{}: {e}", f.display()))?;
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            let text = String::from_utf8(bytes).map_err(|e| format!("{}: {e}", f.display()))?;
            (Some(format!("{:016x}", h.finish())), Some(text))
        }
    };
    let instances: Vec<String> = h
        .kernel
        .agents
        .iter()
        .filter(|a| a.value().record().handler_module.as_deref() == Some(name))
        .map(|a| a.key().0.clone())
        .collect();
    for id in instances {
        // the instance's view, with the BUNDLE's declared fields (kind, calls,
        // bytes_file, tui) over it: an edit of bundle.yaml must reach every
        // instance (an instance's own stale `calls` used to win, and a panel
        // calling a new verb was refused)
        let own = h
            .kernel
            .agents
            .get(&fantastic_kernel::AgentId::from(id.as_str()))
            .and_then(|a| {
                a.value()
                    .record()
                    .meta
                    .get("view")
                    .and_then(Value::as_object)
                    .cloned()
            })
            .unwrap_or_default();
        let bytes = text.as_ref().map(|t| t.replace("{{agent}}", &id));
        let view = merged_view(own, declared.clone(), rev.clone(), bytes);
        let r = h
            .operator(
                "kernel",
                json!({"type": "update_agent", "id": id, "view": view}),
            )
            .await
            .result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(format!("{id}: its view: {e}"));
        }
    }
    Ok(())
}

/// The view an instance gets: the bundle's declared fields over the instance's
/// own, `bytes` from the view file when given (else an inline `bytes` of the
/// bundle or the instance is kept), and `bytes_rev` set when the bundle names a
/// view file.
fn merged_view(
    mut instance: serde_json::Map<String, Value>,
    bundle: serde_json::Map<String, Value>,
    rev: Option<String>,
    bytes: Option<String>,
) -> serde_json::Map<String, Value> {
    instance.extend(bundle);
    if let Some(b) = bytes {
        instance.insert("bytes".into(), json!(b));
    }
    match rev {
        Some(r) => {
            instance.insert("bytes_rev".into(), json!(r));
        }
        None => {
            instance.remove("bytes_rev");
        }
    }
    instance
}

fn err_of(r: &Value) -> Option<String> {
    r.get("error").and_then(Value::as_str).map(str::to_string)
}

/// The new folders whose files were watched BEFORE their bundle loaded (a
/// spawn arms its folder, then loads it): their own `added` event, however late
/// it comes (Windows), has nothing left to load — reloading then restarted the
/// instance made since, mid-call.
static ARMED: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());

/// Watch new bundle folder `name`'s files before it is loaded: an edit from now
/// on is an event, and the folder's own `added` event reloads nothing.
pub async fn arm(h: &Host, name: &str) {
    // marked first: the folder's event may be handled while the watch is armed
    ARMED
        .lock()
        .expect("armed folders")
        .insert(name.to_string());
    if let Some(store) = store_of(h) {
        watch_path(h, &store, &format!("{BUNDLES}/{name}")).await;
    }
}

/// The root loader's discovered store: the one that reports the bundle folders.
fn store_of(h: &Host) -> Option<fantastic_kernel::AgentId> {
    fantastic_kernel::persistence::find_store(&h.kernel)
}

async fn watch_path(h: &Host, store: &fantastic_kernel::AgentId, path: &str) {
    let r = h
        .operator(&store.0, json!({"type": "watch_path", "path": path}))
        .await
        .result;
    if let Some(e) = err_of(&r) {
        tracing::warn!("bundle watch {path}: {e}");
    }
}

/// The bundle a changed path belongs to, and whether it is the bundle's folder
/// itself (added / removed) rather than a file in it.
fn bundle_of(path: &str) -> Option<(String, bool)> {
    let rest = path
        .trim_start_matches("./")
        .strip_prefix(BUNDLES)?
        .trim_start_matches('/');
    let mut parts = rest.split('/');
    let name = parts.next().filter(|n| !n.is_empty())?.to_string();
    Some((name, parts.next().is_none()))
}

/// Watch `<state>/bundles/` for the life of the process — on the disk's events,
/// through the root loader's discovered store.
pub fn spawn(h: Arc<Host>, store_dir: PathBuf) {
    let root = store_dir.join("bundles");
    tokio::spawn(async move {
        if let Err(e) = std::fs::create_dir_all(&root) {
            tracing::warn!("{}: {e}", root.display());
            return;
        }
        let Some(store) = store_of(&h) else {
            tracing::warn!("bundle watch: the root loader has no store");
            return;
        };
        let inbox = fantastic_kernel::AgentId::from(INBOX);
        let mut rx = h.kernel.ensure_inbox(&inbox);
        if let Some(denial) = h.kernel.watch(&store, inbox.clone()).await {
            tracing::warn!("bundle watch: {denial}");
            return;
        }
        watch_path(&h, &store, BUNDLES).await;
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                if let Some(n) = e.file_name().to_str() {
                    watch_path(&h, &store, &format!("{BUNDLES}/{n}")).await;
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
                let Some((name, is_folder)) =
                    p.get("path").and_then(Value::as_str).and_then(bundle_of)
                else {
                    continue;
                };
                let dir = root.join(&name);
                if !dir.is_dir() {
                    if is_folder {
                        // made again later, it is a new folder
                        ARMED.lock().expect("armed folders").remove(&name);
                    }
                    continue; // removed: nothing to load
                }
                if is_folder {
                    // a NEW bundle: watch its files too, and load it — unless it
                    // was armed before its load (`arm`)
                    if !ARMED.lock().expect("armed folders").contains(&name) {
                        watch_path(&h, &store, &format!("{BUNDLES}/{name}")).await;
                        code.insert(name);
                    }
                    continue;
                }
                let file = p.get("path").and_then(Value::as_str).unwrap_or("");
                // python's own bytecode cache, written when an instance FIRST runs
                // its code: not an edit — taken as one, it restarted every instance
                // right after placing (and dropped the panel's first click)
                if Path::new(file)
                    .file_name()
                    .is_some_and(|f| f == "__pycache__")
                {
                    continue;
                }
                let is_view =
                    view_file(&dir).is_some_and(|v| v.file_name() == Path::new(file).file_name());
                if is_view {
                    views.insert(name)
                } else {
                    code.insert(name)
                };
            }
            // a folder with no `bundle.yaml` yet is one being written, not a
            // bundle: nothing to load (its `bundle.yaml` arriving is the event)
            code.retain(|name| root.join(name).join("bundle.yaml").is_file());
            views.retain(|name| root.join(name).join("bundle.yaml").is_file());
            for name in &code {
                let r = h
                    .operator("kernel", json!({"type": "reload_bundle", "name": name}))
                    .await
                    .result;
                if let Some(e) = err_of(&r) {
                    tracing::warn!("bundle {name}: reload: {e}");
                }
            }
            // every changed bundle's instances get its view again — a save that
            // touches the code AND the panel at once (an agent rewriting the
            // bundle) reloads the code and THEN redraws the panel; skipping the
            // panel there left the starter's old panel calling verbs that were gone
            for name in code.union(&views) {
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
        (
            "bundle.yaml",
            include_str!("../../starters/python_agent/bundle.yaml"),
        ),
        (
            "agent.py",
            include_str!("../../starters/python_agent/agent.py"),
        ),
        (
            "panel.html",
            include_str!("../../starters/python_agent/panel.html"),
        ),
        (
            "fantastic_agent.py",
            include_str!(
                "../../../lib/python/bundled_agents/io/py_agent/src/py_agent/fantastic_agent.py"
            ),
        ),
    ],
)];

/// Copy starter `kind` into `<store>/bundles/<name>/` (a NEW folder — refused if
/// it exists). Returns the folder.
pub fn scaffold(store: &Path, kind: &str, name: &str) -> Result<PathBuf, String> {
    let files = STARTERS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, f)| *f)
        .ok_or_else(|| {
            format!(
                "no starter `{kind}` — the starters are {}",
                STARTERS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use fantastic_kernel::bundle::{BundleError, Handler};
    use fantastic_kernel::{Agent, Kernel};

    struct Plain;

    fantastic_kernel::stand_in_verbs!(plain_verbs { Ping "ping" {} });

    #[async_trait]
    impl Handler for Plain {
        type Call = plain_verbs::Call;

        fn name(&self) -> &str {
            "board"
        }

        fn man(&self) -> &'static str {
            "A test stand-in bundle."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            plain_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            plain_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }

        async fn handle(
            &self,
            _id: &fantastic_kernel::AgentId,
            call: Self::Call,
            _k: &Arc<Kernel>,
        ) -> Result<Option<Value>, BundleError> {
            Ok(match call {
                plain_verbs::Call::Ping(_) => Some(json!({"pong": true})),
                plain_verbs::Call::Reflect(_) => None,
            })
        }
    }

    /// A project dir with bundle `board` (its `bundle.yaml` = `record.view` as
    /// given) and a host with one instance of it, `board_1`, holding `own` as its view.
    async fn world(name: &str, view: &Value, own: Option<Value>) -> (Host, PathBuf) {
        let dir = std::env::temp_dir().join(format!("fc-bw-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bundle = dir.join(".fantastic/bundles/board");
        std::fs::create_dir_all(&bundle).unwrap();
        write_bundle(&bundle, view);
        std::fs::write(bundle.join("panel.html"), "<p>one</p>").unwrap();
        let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
            state_dir: dir.join(".fantastic"),
            work_dir: dir.to_path_buf(),
        });
        kernel.bundles.register(
            fantastic_core::HANDLER_MODULE,
            fantastic_core::KernelStateBundle::default(),
        );
        let kernel = Arc::new(kernel);
        kernel
            .bundles
            .register_folder("board", Arc::new(Plain), json!({}));
        let root = Agent::new(
            fantastic_kernel::AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            serde_json::Map::new(),
            dir.join(".fantastic"),
            false,
        );
        kernel.register(Arc::clone(&root));
        kernel.set_root(root);
        let h = Host {
            kernel,
            cfg: crate::env::ClientCfg::here(),
            root: "kernel".to_string(),
        };
        let mut spec = json!({"type": "create_agent", "parent_id": "kernel", "id": "board_1", "handler_module": "board"});
        if let Some(v) = own {
            spec["view"] = v;
        }
        let r = h.operator("kernel", spec).await.result;
        assert!(r.get("error").is_none(), "{r}");
        (h, bundle)
    }

    fn write_bundle(dir: &Path, view: &Value) {
        let doc = json!({"base": "py_agent", "sentence": "s", "record": {"view": view}});
        std::fs::write(
            dir.join("bundle.yaml"),
            serde_yaml::to_string(&doc).unwrap(),
        )
        .unwrap();
    }

    fn view_of(h: &Host) -> Value {
        h.kernel
            .agents
            .get(&fantastic_kernel::AgentId::from("board_1"))
            .and_then(|a| a.value().record().meta.get("view").cloned())
            .unwrap_or(Value::Null)
    }

    fn tui(verb: &str) -> Value {
        json!([{"title": "S", "call": {"verb": verb}, "show": "kv"}])
    }

    #[tokio::test]
    async fn a_bundle_declaration_with_only_tui_reaches_its_instances() {
        let (h, dir) = world("only-tui", &json!({"tui": tui("status")}), None).await;
        refresh_views(&h, "board", &dir).await.unwrap();
        let v = view_of(&h);
        assert_eq!(v["tui"], tui("status"));
        assert!(
            v.get("bytes_rev").is_none() && v.get("bytes").is_none(),
            "{v}"
        );
    }

    #[tokio::test]
    async fn editing_tui_in_bundle_yaml_reaches_a_live_instance() {
        let (h, dir) = world("edit", &json!({"tui": tui("status")}), None).await;
        refresh_views(&h, "board", &dir).await.unwrap();
        write_bundle(&dir, &json!({"tui": tui("other")}));
        refresh_views(&h, "board", &dir).await.unwrap();
        assert_eq!(view_of(&h)["tui"], tui("other"));
    }

    #[tokio::test]
    async fn a_web_and_tui_declaration_in_one_view_reach_the_instance_together() {
        let view = json!({"kind": "html", "bytes_file": "panel.html", "calls": ["status"], "tui": tui("status")});
        let (h, dir) = world("both", &view, None).await;
        refresh_views(&h, "board", &dir).await.unwrap();
        let v = view_of(&h);
        assert_eq!(v["kind"], "html");
        assert_eq!(v["bytes_file"], "panel.html");
        assert_eq!(v["calls"], json!(["status"]));
        assert_eq!(v["tui"], tui("status"));
        assert_eq!(v["bytes"], "<p>one</p>");
        assert!(
            v["bytes_rev"].as_str().is_some_and(|r| r.len() == 16),
            "{v}"
        );
    }

    #[tokio::test]
    async fn the_view_files_agent_placeholder_is_the_instance_id() {
        let view = json!({"kind": "html", "bytes_file": "panel.html"});
        let (h, dir) = world("agent", &view, None).await;
        std::fs::write(dir.join("panel.html"), "<p>{{agent}}</p>").unwrap();
        refresh_views(&h, "board", &dir).await.unwrap();
        assert_eq!(view_of(&h)["bytes"], "<p>board_1</p>");
    }

    #[test]
    fn bundle_of_reads_paths_relative_to_the_store() {
        assert_eq!(
            bundle_of("bundles/board/panel.html"),
            Some(("board".to_string(), false))
        );
        assert_eq!(
            bundle_of("bundles/board"),
            Some(("board".to_string(), true))
        );
    }

    #[tokio::test]
    async fn an_invalid_tui_in_bundle_yaml_is_refused_and_the_instance_keeps_its_view() {
        let own = json!({"kind": "html", "tui": tui("kept")});
        let (h, dir) = world(
            "bad",
            &json!({"tui": [{"title": "S", "call": {"verb": "v"}, "show": "grid"}]}),
            Some(own.clone()),
        )
        .await;
        let e = refresh_views(&h, "board", &dir).await.expect_err("refused");
        assert!(e.contains("view.tui[0].show") && e.contains("grid"), "{e}");
        assert_eq!(view_of(&h), own, "the instance keeps its view");
    }

    #[test]
    fn merged_view_carries_the_view_files_bytes() {
        let obj = |v: Value| v.as_object().unwrap().clone();
        let own = obj(
            json!({"kind": "old", "extra": 1, "bytes": "x", "bytes_rev": "old", "calls": ["a"]}),
        );
        let bundle = obj(json!({"kind": "html", "calls": ["b"], "tui": tui("v")}));
        let m = merged_view(
            own.clone(),
            bundle.clone(),
            Some("rev".into()),
            Some("B".into()),
        );
        assert_eq!(
            Value::Object(m),
            json!({"kind": "html", "extra": 1, "calls": ["b"], "tui": tui("v"), "bytes_rev": "rev", "bytes": "B"})
        );
        // no view file: no revision (a stale one is dropped), an inline `bytes` is kept
        let m = merged_view(own, bundle, None, None);
        assert!(m.get("bytes_rev").is_none() && m["bytes"] == "x" && m["extra"] == 1);
    }
}
