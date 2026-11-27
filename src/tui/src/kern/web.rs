//! THE CANVAS'S AGENTS — the blueprint's records (grant model v9, `initial-tree.md`):
//!
//! ```text
//! user
//! ├── files
//! │   └── static_files    file_bridge view   THE files every view's cells load:
//! │                                          GET /static_files/file/<path>
//! ├── web                 web.tools          the HTTP host, loopback, `port` picked once
//! │   ├── canvas_ws       web_ws.tools       the page's leg: the person through the page
//! │   ├── canvas_page     canvas_page.tools  THE CANVAS: its page, embedded in the binary
//! │   └── mcp             web_mcp.tools      the coding CLI's leg
//! └── canvas_root         canvas.tools       THE USER SPACE: the main canvas's node (`main: true`)
//!     ├── web_loader      kernel_state       its page records (@state/web)
//!     ├── canvas_garage   canvas.tools       the garage, an open sub-canvas
//!     │   └── canvas_garage_loader
//!     └── canvas_<name>   canvas.tools       a CHILD canvas (`new_canvas` sent to a canvas):
//!         │                                  its page records nest under its parent's
//!         ├── canvas_<name>_loader            its page records
//!         └── canvas_<sub>  …                 a canvas it spawned, the same way
//!
//! A canvas's CHILDREN are the canvases it spawned: deleting a canvas deletes its
//! whole subtree, the canvases it spawned with it.
//!
//! No host id here is a page id: the page's records (`canvas`, `view_root`, …)
//! resolve against the host tree in the page loader.
//! ```
//!
//! THE LEG HOLDS ITS OWN LINES: `canvas_ws` creates in the user space and reads the
//! work folder by the lines in its record, capped by `web`'s. No code writes a line.
//!
//! `port`: the first boot picks a free port, or takes `--port N`, and records it on
//! `web`; later runs read it from the record, and a different `--port` overwrites it
//! (only when no kernel runs here — a running one refuses every argument, `run.rs`).
//!
//! `web` is the core listener: it listens in every mode (`Host::boot`); `fantastic
//! canvas` only checks the canvas's agents are here and names its URL.

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

pub const WEB_ID: &str = "web";
pub const CANVAS_PAGE_ID: &str = "canvas_page";
pub const CANVAS_WS_ID: &str = "canvas_ws";
/// The canvas page addresses its loader by this id (`src/tui/web/canvas`).
pub const CANVAS_LOADER_ID: &str = "web_loader";
/// The canvas's host node, under `user` — NOT `canvas`: a host id the page's
/// records also use would make the page loader file them under the host agent
/// (ids resolve against the host tree).
pub const CANVAS_NODE_ID: &str = "canvas_root";
/// The canvas's one file view, under `files`: every view's cells load
/// `GET /static_files/file/<path>` from it.
pub const STATIC_FILES_ID: &str = "static_files";

fn meta(h: &Host, id: &str) -> Map<String, Value> {
    h.kernel.resolve_target(id)
        .map(|a| a.record().meta.clone())
        .unwrap_or_default()
}

async fn create(h: &Host, spec: Value) -> Result<(), String> {
    let id = spec["id"].as_str().unwrap_or("?").to_string();
    let r = h.operator("kernel", fantastic_kernel::verbs::call("create_agent", spec)).await.result;
    fantastic_kernel::refusal::error_text(&r).map_or(Ok(()), |e| Err(format!("could not create `{id}`: {e}")))
}

/// A CHILD CANVAS (phase 4): a `canvas.tools` node under `parent` (the canvas that
/// spawned it) — its own page kernel (the page records under `loader`, rooted at
/// `root`), its own cells' backends as its children. `main` is declarative only:
/// the page opens the main canvas first and lists it on top. Creates what is
/// missing (`create_agent` boots each).
pub async fn ensure_canvas(
    h: &Host,
    parent: &str,
    node: &str,
    loader: &str,
    root: &str,
    main: bool,
) -> Result<(), String> {
    if !h.kernel.exists(node) {
        create(
            h,
            json!({"parent_id": parent, "id": node,
            "handler_module": crate::kern::backend::HANDLER_MODULE, "main": main}),
        )
        .await?;
    }
    if !h.kernel.exists(loader) {
        create(h, json!({"parent_id": node, "id": loader, "handler_module": fantastic_core::HANDLER_MODULE,
            "root": root, "watch": false})).await?;
    }
    // its page bundles' folder exists (no watching), so the page's bundles
    // have a place to be put (the folder is the loader's `root` under the kernel's one rule)
    let (base, rel) = h.kernel.record_root(root, std::path::Path::new(""))?;
    let bundles = base.join(rel).join("bundles");
    if let Err(e) = std::fs::create_dir_all(&bundles) {
        return Err(format!("{}: {e}", bundles.display()));
    }
    Ok(())
}

/// `node`'s page loader: its `kernel_state` child.
fn loader_of(h: &Host, node: &str) -> Option<String> {
    h.kernel.agent_ids().into_iter().filter_map(|aid| h.kernel.resolve_target(aid.as_str()))
        .find(|l| {
            let r = l.record();
            r.parent_id.as_deref() == Some(node)
                && r.handler_module.as_deref() == Some(fantastic_core::HANDLER_MODULE)
        })
        .map(|l| l.id.0.clone())
}

/// Every canvas: `(node, loader)` — the main canvas under `user`, and each canvas
/// nested under a canvas (`new_canvas` on it), at any depth — with its page loader
/// (its `kernel_state` child).
pub fn canvases(h: &Host) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut parents = vec![crate::env::HUMAN.to_string()];
    while let Some(parent) = parents.pop() {
        for a in h.kernel.agent_ids().into_iter().filter_map(|aid| h.kernel.resolve_target(aid.as_str())) {
            let r = a.record();
            if r.parent_id.as_deref() != Some(parent.as_str())
                || r.handler_module.as_deref() != Some(crate::kern::backend::HANDLER_MODULE)
            {
                continue;
            }
            let node = a.id.0.clone();
            if let Some(loader) = loader_of(h, &node) {
                out.push((node.clone(), loader));
            }
            parents.push(node);
        }
    }
    out.sort();
    out
}

/// The page records dir of a canvas `node` spawned by a canvas whose own records
/// dir is `parent_root`: nested under it, `<parent_root>/<node>`.
pub fn child_records_root(parent_root: &str, node: &str) -> String {
    format!("{parent_root}/{node}")
}

/// `new_canvas {name}` (`canvas.tools`, sent to the canvas `parent`): a CHILD
/// canvas `canvas_<name>` of `parent`, its page records nested under the parent's
/// records dir. Returns its node id. A name taken anywhere is refused.
pub async fn new_canvas(h: &Host, parent: &str, name: &str) -> Result<String, String> {
    let canvas_parent = h
        .kernel.resolve_target(parent)
        .is_some_and(|a| {
            a.record().handler_module.as_deref()
                == Some(crate::kern::backend::HANDLER_MODULE)
        });
    if !canvas_parent {
        return Err(format!("new_canvas: `{parent}` is not a canvas"));
    }
    let node = format!("canvas_{name}");
    if h.kernel.exists(&node) {
        return Err(format!("a canvas `{node}` already exists"));
    }
    let parent_loader =
        loader_of(h, parent).ok_or_else(|| format!("new_canvas: `{parent}` has no page loader"))?;
    let parent_root = meta(h, &parent_loader)
        .get("root")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("new_canvas: the page loader of `{parent}` has no root"))?;
    let loader = format!("{node}_loader");
    // `create_agent` boots each as it makes it
    ensure_canvas(
        h,
        parent,
        &node,
        &loader,
        &child_records_root(&parent_root, &node),
        false,
    )
    .await?;
    Ok(node)
}

/// A port free on this machine now (the OS picks it), for the first run that names none.
pub(crate) fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("no free port to pick: {e}"))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("no free port to pick: {e}"))
}

/// What `ensure` serves: the canvas's page, on its port.
pub struct Urls {
    pub canvas: String,
    pub port: u16,
}

/// A wildcard listener address (`0.0.0.0`, `::`): it listens on every address, and a
/// browser cannot dial it.
pub fn is_wildcard(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::")
}

/// The authority a browser dials for a `web` listening at `host` on `port`. A wildcard
/// is reached at `localhost`, which the Host guard admits on the loopback identity
/// (`fantastic_web`'s `origin.rs`); an IPv6 literal is bracketed, as a URL spells it;
/// any other address is itself.
pub fn canvas_authority(host: &str, port: u16) -> String {
    match host {
        h if is_wildcard(h) => format!("localhost:{port}"),
        h if h.contains(':') => format!("[{h}]:{port}"),
        h => format!("{h}:{port}"),
    }
}

/// THE RUN FILE (owner 2026-10-16, for a local manager of fantastics): while this
/// process serves the canvas, `{pid, port, url}` — written once it serves,
/// removed at a clean exit. In the state folder (`<state>/cli/canvas.yaml`). A killed process leaves it
/// behind: the lock (`<state>/lock.yaml`, flocked while the kernel runs) says
/// whether it is live.
pub const RUN_FILE: &str = fantastic_home::RUN_FILE;

/// Write `state`'s [`RUN_FILE`] for this process.
pub fn write_run_file(state: &std::path::Path, urls: &Urls) -> Result<(), String> {
    crate::atomic::write_yaml(
        &state.join(RUN_FILE),
        &json!({"pid": std::process::id(), "port": urls.port, "url": urls.canvas}),
    )
}

/// Remove `state`'s [`RUN_FILE`] at a clean exit (none written: nothing to remove).
pub fn remove_run_file(state: &std::path::Path) {
    let _ = std::fs::remove_file(state.join(RUN_FILE));
}

/// Check the canvas's agents are in the tree and the `--port`/`--host` asked for are
/// what `web` holds (`Host::boot` set them before the listener booted), make each
/// canvas's page bundles folder, and return the page's URL. Creates no agent and
/// writes no line: they are the blueprint's records.
pub async fn ensure(h: &Host, port: Option<u16>, bind: Option<&str>) -> Result<Urls, String> {
    let recorded = meta(h, WEB_ID)
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16);
    // the address the `web` listens on (its `bind`, else its `host`)
    let bound = fantastic_web::listen_host(&meta(h, WEB_ID));
    if let Some(b) = bind {
        if bound != b {
            return Err(format!("--host {b}: the `web` leg still binds {bound}"));
        }
    }
    let port = match (recorded, port) {
        // Repeating the recorded port changes nothing, so it is not a conflict.
        (Some(r), Some(p)) if r == p => p,
        // `Host::boot` already set a new --port on the record before the
        // listener booted; a mismatch here means that write was refused.
        (r, Some(p)) => {
            return Err(format!(
                "--port {p}: the `web` leg still serves port {} (its record refused the new port)",
                r.map_or("?".into(), |r| r.to_string())
            ))
        }
        (Some(r), None) => r,
        (None, None) => return Err("the `web` record has no port".into()),
    };
    for id in [WEB_ID, CANVAS_PAGE_ID, CANVAS_WS_ID, CANVAS_NODE_ID, CANVAS_LOADER_ID, STATIC_FILES_ID] {
        if !h.kernel.exists(id) {
            return Err(format!("the canvas's `{id}` is not in the tree (the blueprint seeds it)"));
        }
    }
    // every canvas's page bundles folder exists (no watching), so the page's
    // bundles have a place to be put
    for (_, loader) in canvases(h) {
        if let Some(root) = meta(h, &loader).get("root").and_then(Value::as_str) {
            let (base, rel) = h.kernel.record_root(root, std::path::Path::new(""))?;
            let bundles = base.join(rel).join("bundles");
            if let Err(e) = std::fs::create_dir_all(&bundles) {
                return Err(format!("{}: {e}", bundles.display()));
            }
        }
    }
    Ok(Urls {
        canvas: format!(
            "http://{}/{CANVAS_PAGE_ID}/file/index.html",
            canvas_authority(&bound, port)
        ),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_canvas_records_nest_under_its_parents_records_dir() {
        // the main canvas's records are `@state/web`: its child's sit inside them
        let under_main = child_records_root("@state/web", "canvas_child_a");
        assert_eq!(under_main, "@state/web/canvas_child_a");
        // a grandchild nests under its parent's, one level deeper
        assert_eq!(
            child_records_root(&under_main, "canvas_eq"),
            "@state/web/canvas_child_a/canvas_eq"
        );
    }

    #[test]
    fn a_wildcard_bind_is_reached_at_localhost_and_other_addresses_are_themselves() {
        // a browser cannot dial a wildcard: the guard admits `localhost` for it
        assert_eq!(canvas_authority("0.0.0.0", 8088), "localhost:8088");
        assert_eq!(canvas_authority("::", 8088), "localhost:8088");
        // a loopback bind keeps the address it names
        assert_eq!(canvas_authority("127.0.0.1", 8123), "127.0.0.1:8123");
        assert_eq!(canvas_authority("localhost", 8123), "localhost:8123");
        // a LAN address answers itself; an IPv6 literal is bracketed as a URL spells it
        assert_eq!(canvas_authority("192.168.1.5", 8088), "192.168.1.5:8088");
        assert_eq!(canvas_authority("::1", 8088), "[::1]:8088");
    }

    #[test]
    fn the_run_file_names_this_process_its_port_and_url_until_a_clean_exit() {
        let d = std::env::temp_dir().join(format!("fc-runfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let urls = Urls {
            canvas: "http://127.0.0.1:8123/canvas_page/file/index.html".into(),
            port: 8123,
        };
        write_run_file(&d, &urls).unwrap();
        let v = crate::atomic::read_yaml(&d.join(RUN_FILE)).unwrap();
        assert_eq!(
            v,
            json!({"pid": std::process::id(), "port": 8123, "url": urls.canvas})
        );
        remove_run_file(&d);
        assert!(!d.join(RUN_FILE).exists());
        remove_run_file(&d); // nothing left: still fine
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The canvas tree on a real kernel: `new_canvas` nests a child canvas under the canvas
/// that calls it, its page records under that canvas's records, a taken name is refused
/// anywhere, deleting a canvas deletes the canvases it spawned, and the listing reaches
/// nested and flat peer canvases alike.
#[cfg(test)]
pub(crate) mod canvas_tree_tests {
    use super::*;
    use crate::env::ClientCfg;
    use fantastic_kernel::{Agent, AgentId as KernelId, Kernel};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// A scratch project dir for one test (absolute paths: the page records resolve there).
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fc-canvas-tree-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A host whose root is `kernel` (the kernel-state root the canvas tree is made under).
    pub(crate) fn harness(dir: &Path) -> Host {
        let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
            state_dir: dir.join(".fantastic"),
            work_dir: dir.to_path_buf(),
        });
        kernel.bundles.register(
            fantastic_core::HANDLER_MODULE,
            fantastic_core::KernelStateBundle::default(),
        );
        kernel.bundles.register(
            crate::kern::backend::HANDLER_MODULE,
            crate::kern::backend::BackendBundle,
        );
        // the agents the canvas world creates (`create_agent` refuses a handler that is not installed)
        kernel.bundles.register(
            fantastic_user::HANDLER_MODULE,
            fantastic_user::UserBundle::default(),
        );
        kernel.bundles.register(
            fantastic_canvas_page::HANDLER_MODULE,
            fantastic_canvas_page::CanvasPageBundle,
        );
        let kernel = Arc::new(kernel);
        let root = Agent::new(
            KernelId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            Map::new(),
            dir.join(".fantastic"),
            false,
        );
        fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
        kernel.set_root(root);
        Host {
            kernel,
            cfg: ClientCfg::here(),
            root: "kernel".to_string(),
        }
    }

    /// The main canvas as the blueprint seeds it, under `user`: its records under `dir`.
    async fn canvas_world(dir: &Path) -> Host {
        let h = harness(dir);
        for spec in [
            json!({"parent_id": "kernel", "id": crate::env::HUMAN, "handler_module": "user.tools"}),
            json!({"parent_id": "kernel", "id": CANVAS_PAGE_ID, "handler_module": "canvas_page.tools"}),
        ] {
            create(&h, spec).await.unwrap();
        }
        ensure_canvas(
            &h,
            crate::env::HUMAN,
            CANVAS_NODE_ID,
            CANVAS_LOADER_ID,
            "@state/web",
            true,
        )
        .await
        .unwrap();
        h
    }

    fn parent_of(h: &Host, id: &str) -> Option<String> {
        h.kernel.resolve_target(KernelId::from(id).as_str())
            .and_then(|a| a.record().parent_id.clone())
    }

    fn records_root_of(h: &Host, loader: &str) -> String {
        meta(h, loader)
            .get("root")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn a_canvas_makes_its_new_canvas_as_its_child_with_records_nested_under_its_own() {
        let dir = scratch("nested");
        let h = canvas_world(&dir).await;
        let node = new_canvas(&h, CANVAS_NODE_ID, "research").await.unwrap();
        assert_eq!(node, "canvas_research");
        assert_eq!(
            parent_of(&h, &node).as_deref(),
            Some(CANVAS_NODE_ID),
            "a CHILD of the calling canvas"
        );
        assert_eq!(
            h.kernel.resolve_target(node.as_str())
                .unwrap()
                .record()
                .handler_module
                .as_deref(),
            Some(crate::kern::backend::HANDLER_MODULE)
        );
        let loader = loader_of(&h, &node).expect("its own page loader");
        assert_eq!(loader, "canvas_research_loader");
        assert_eq!(records_root_of(&h, &loader), "@state/web/canvas_research");
        assert!(
            dir.join(".fantastic/web/canvas_research/bundles").is_dir(),
            "its bundles folder exists"
        );
        // a canvas made by that child nests one level deeper, under the child's records
        let deeper = new_canvas(&h, &node, "deeper").await.unwrap();
        assert_eq!(parent_of(&h, &deeper).as_deref(), Some("canvas_research"));
        assert_eq!(
            records_root_of(&h, &loader_of(&h, &deeper).unwrap()),
            "@state/web/canvas_research/canvas_deeper"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_name_taken_anywhere_in_the_tree_is_refused_and_nothing_is_made() {
        let dir = scratch("clash");
        let h = canvas_world(&dir).await;
        new_canvas(&h, CANVAS_NODE_ID, "research").await.unwrap();
        // the same name under the same canvas
        let err = new_canvas(&h, CANVAS_NODE_ID, "research")
            .await
            .unwrap_err();
        assert!(
            err.contains("canvas_research") && err.contains("already exists"),
            "{err}"
        );
        // a name the main canvas already holds, asked from a nested canvas
        let err = new_canvas(&h, "canvas_research", "root").await.unwrap_err();
        assert!(
            err.contains("canvas_root") && err.contains("already exists"),
            "{err}"
        );
        assert_eq!(
            h.kernel.agent_ids().into_iter().filter_map(|aid| h.kernel.resolve_target(aid.as_str()))
                .filter(|a| a.id.0 == "canvas_research_loader")
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `web` record as a recorded run left it: its port, and its `bind` (or `host`) when `--host` named one.
    /// Kernel-state handled, so the tree boots no listener here.
    async fn recorded_web(h: &Host, extra: Value) {
        let mut spec = json!({"parent_id": "kernel", "id": WEB_ID, "handler_module": fantastic_core::HANDLER_MODULE, "port": 8088});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            spec[k] = v;
        }
        create(h, spec).await.unwrap();
    }

    #[tokio::test]
    async fn a_recorded_web_refuses_a_bind_it_does_not_hold() {
        let dir = scratch("bind-refused");
        let h = harness(&dir);
        recorded_web(&h, json!({"bind": "0.0.0.0"})).await;
        // another bind than the record's: refused before anything is created or booted
        let Err(err) = ensure(&h, None, Some("127.0.0.1")).await else {
            panic!("a bind the record does not hold is refused")
        };
        assert!(
            err.contains("--host 127.0.0.1") && err.contains("still binds 0.0.0.0"),
            "{err}"
        );
        // a recorded web with no host binds the default: `--host 0.0.0.0` is refused against it
        let plain = scratch("bind-refused-default");
        let h2 = harness(&plain);
        recorded_web(&h2, json!({})).await;
        let Err(err) = ensure(&h2, None, Some("0.0.0.0")).await else {
            panic!("refused")
        };
        assert!(
            err.contains("--host 0.0.0.0") && err.contains("still binds 127.0.0.1"),
            "{err}"
        );
        // a record from before `bind` names its listener in `host`, and that is the bind it holds
        let legacy = scratch("bind-refused-legacy");
        let h3 = harness(&legacy);
        recorded_web(&h3, json!({"host": "0.0.0.0"})).await;
        let Err(err) = ensure(&h3, None, Some("127.0.0.1")).await else {
            panic!("refused")
        };
        assert!(err.contains("still binds 0.0.0.0"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&plain);
        let _ = std::fs::remove_dir_all(&legacy);
    }

    #[tokio::test]
    async fn only_a_canvas_spawns_a_canvas_and_a_name_is_required() {
        let dir = scratch("refuse");
        let h = canvas_world(&dir).await;
        let err = new_canvas(&h, CANVAS_PAGE_ID, "x").await.unwrap_err();
        assert!(err.contains("is not a canvas"), "{err}");
        assert!(!h.kernel.exists(KernelId::from("canvas_x").as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deleting_a_canvas_deletes_the_canvases_it_spawned() {
        let dir = scratch("cascade");
        let h = canvas_world(&dir).await;
        let node = new_canvas(&h, CANVAS_NODE_ID, "research").await.unwrap();
        let deeper = new_canvas(&h, &node, "deeper").await.unwrap();
        let (loader, deeper_loader) = (format!("{node}_loader"), format!("{deeper}_loader"));
        let out = h
            .operator("kernel", json!({"type": "delete_agent", "id": node}))
            .await;
        assert!(out.result.get("error").is_none(), "{:?}", out.result);
        for gone in [&node, &loader, &deeper, &deeper_loader] {
            assert!(
                !h.kernel.exists(KernelId::from(gone.as_str()).as_str()),
                "{gone} is gone"
            );
        }
        assert!(
            h.kernel.exists(KernelId::from(CANVAS_NODE_ID).as_str()),
            "the main canvas stays"
        );
        assert_eq!(
            canvases(&h),
            vec![(CANVAS_NODE_ID.to_string(), CANVAS_LOADER_ID.to_string())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_listing_reaches_the_main_canvas_and_every_nested_one() {
        let dir = scratch("listing");
        let h = canvas_world(&dir).await;
        let nested = new_canvas(&h, CANVAS_NODE_ID, "research").await.unwrap();
        let nested_deeper = new_canvas(&h, &nested, "deeper").await.unwrap();
        let listed = canvases(&h);
        let expect = vec![
            (CANVAS_NODE_ID.to_string(), CANVAS_LOADER_ID.to_string()),
            (nested.clone(), format!("{nested}_loader")),
            (nested_deeper.clone(), format!("{nested_deeper}_loader")),
        ];
        let mut expect = expect;
        expect.sort();
        assert_eq!(listed, expect);
        // every listed canvas is reachable: a canvas node whose loader is its page's
        for (node, loader) in &listed {
            let hm = h
                .kernel.resolve_target(node.as_str())
                .unwrap()
                .record()
                .handler_module
                .clone();
            assert_eq!(
                hm.as_deref(),
                Some(crate::kern::backend::HANDLER_MODULE),
                "{node} is a canvas"
            );
            assert_eq!(
                loader_of(&h, node).as_deref(),
                Some(loader.as_str()),
                "{node} boots its own loader"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_canvas_that_is_not_under_user_is_not_listed() {
        let dir = scratch("unlisted");
        let h = canvas_world(&dir).await;
        // a canvas.tools node under the operator's own user, not under any canvas
        create(&h, json!({"parent_id": "kernel", "id": "stray", "handler_module": crate::kern::backend::HANDLER_MODULE})).await.unwrap();
        create(&h, json!({"parent_id": "stray", "id": "stray_loader", "handler_module": fantastic_core::HANDLER_MODULE, "root": "x", "watch": false})).await.unwrap();
        assert!(!canvases(&h).iter().any(|(n, _)| n == "stray"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
