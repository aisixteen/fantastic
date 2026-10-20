//! THE CANVAS'S AGENTS, created by code on `fantastic canvas` (never by the
//! blueprint).
//!
//! ```text
//! user
//! └── web                 web.tools          the HTTP host, loopback, `port` from --port
//!     ├── canvas_ws       web_ws.tools       the canvas's socket leg
//!     └── canvas_page     canvas_page.tools  THE CANVAS: its page, embedded in the binary
//!         ├── canvas_root canvas.tools       THE MAIN canvas's node (`main: true`):
//!         │   │                              creates / deletes its cells'
//!         │   │                              backends (AIs, exec agents)
//!         │   ├── static_files  file_bridge  THE files every view's cells load:
//!         │   │                              GET /static_files/file/<path>
//!         │   ├── web_loader  kernel_state   its page records (.fantastic/web)
//!         │   └── canvas_<name>  canvas.tools a CHILD canvas (`new_canvas` sent to
//!         │       │                          canvas_root): its page records nest under
//!         │       │                          its parent's (.fantastic/web/canvas_<name>)
//!         │       ├── canvas_<name>_loader    its page records
//!         │       └── canvas_<sub>  …         a canvas it spawned, the same way
//!         └── (a flat peer canvas made before nesting loads as it did, under canvas_page)
//!
//! A canvas's CHILDREN are the canvases it spawned: deleting a canvas deletes its
//! whole subtree, the canvases it spawned with it.
//!
//! No host id here is a page id: the page's records (`canvas`, `view_root`, …)
//! resolve against the host tree in the page loader.
//! ```
//!
//! THE LEG REACHES ONLY `user` (owner, 2026-10-03/04): a tab's every host call
//! is `user.forward`, and `user` makes it — the shell's own identity, god mode by grants
//! (`kern/reach`).
//!
//! THE CANVAS (design 33) replaced the browser View of the terminal shell
//! (owner, 2026-09-30): its page is served by `canvas_page` through `web`'s
//! `/file/` route; the page speaks through `canvas_ws` to `user`.
//!
//! `port`: the first `fantastic canvas` takes `--port N`, or picks a free port; later runs read it
//! from `web`'s record, and a different `--port` overwrites it (only when no
//! kernel runs here — a running one refuses every argument, `run.rs`).

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

pub const WEB_ID: &str = "web";
pub const CANVAS_PAGE_ID: &str = "canvas_page";
pub const CANVAS_WS_ID: &str = "canvas_ws";
/// The canvas page addresses its loader by this id (`src/tui/web/canvas`).
pub const CANVAS_LOADER_ID: &str = "web_loader";
/// The canvas's host node, under `canvas_page` — NOT `canvas`: a host id the
/// page's records also use would make the page loader file them under the host
/// agent (ids resolve against the host tree).
pub const CANVAS_NODE_ID: &str = "canvas_root";
/// The canvas's one file leg, under `canvas_root`: every view's cells load
/// `GET /static_files/file/<path>` from it.
pub const STATIC_FILES_ID: &str = "static_files";

fn err_of(r: &fantastic_kernel::Outcome) -> Option<String> {
    r.result.get("error").and_then(Value::as_str).map(str::to_string)
}

fn exists(h: &Host, id: &str) -> bool {
    h.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(id))
}

fn meta(h: &Host, id: &str) -> Map<String, Value> {
    h.kernel
        .agents
        .get(&fantastic_kernel::AgentId::from(id))
        .map(|a| a.value().record().meta.clone())
        .unwrap_or_default()
}

async fn merge_verbs(h: &Host, id: &str, field: &str, add: &str, verbs: Value) -> Result<(), String> {
    let mut g = meta(h, id).get(field).and_then(Value::as_object).cloned().unwrap_or_default();
    g.insert(add.into(), verbs);
    let r = h.operator("kernel", json!({"type":"update_agent","id":id, field: g})).await;
    err_of(&r).map_or(Ok(()), |e| Err(format!("could not grant {id}.{field} += {add}: {e}")))
}

async fn create(h: &Host, spec: Value) -> Result<(), String> {
    let id = spec["id"].as_str().unwrap_or("?").to_string();
    let mut spec = spec;
    spec["type"] = json!("create_agent");
    let r = h.operator("kernel", spec).await;
    err_of(&r).map_or(Ok(()), |e| Err(format!("could not create `{id}`: {e}")))
}

/// A CANVAS (phase 4): a `canvas.tools` node under `parent` (`canvas_page`, or the
/// canvas that spawned it) —
/// `canvas_page` — its own page kernel (the page records under `loader`, rooted
/// at `root`), its own cells' backends as its children. `main` is declarative
/// only: the page opens the main canvas first and lists it on top. Creates
/// what is missing (booted by the caller).
pub async fn ensure_canvas(h: &Host, parent: &str, node: &str, loader: &str, root: &str, main: bool) -> Result<(), String> {
    if !exists(h, node) {
        create(h, json!({"parent_id": parent, "id": node,
            "handler_module": crate::kern::backend::HANDLER_MODULE, "main": main})).await?;
    }
    if !exists(h, loader) {
        create(h, json!({"parent_id": node, "id": loader, "handler_module": fantastic_core::HANDLER_MODULE,
            "root": root, "watch": false})).await?;
    }
    // its page bundles' folder exists, so the page can WATCH it (`bundle_watch`)
    if let Err(e) = std::fs::create_dir_all(format!("{root}/bundles")) {
        return Err(format!("{root}/bundles: {e}"));
    }
    // `user` reads and writes the page records (every tab's call is `user`'s)
    let user = crate::env::HUMAN;
    merge_verbs(h, user, "can_call", loader, json!(["*"])).await?;
    merge_verbs(h, loader, "callable_by", user, json!(["*"])).await
}

async fn boot_canvas(h: &Host, node: &str, loader: &str) -> Result<(), String> {
    for id in [node, loader] {
        let r = h.operator(id, json!({"type":"boot"})).await;
        if let Some(e) = err_of(&r) {
            return Err(format!("`{id}` did not boot: {e}"));
        }
    }
    Ok(())
}

/// `node`'s page loader: its `kernel_state` child.
fn loader_of(h: &Host, node: &str) -> Option<String> {
    h.kernel
        .agents
        .iter()
        .find(|l| {
            let r = l.value().record();
            r.parent_id.as_deref() == Some(node) && r.handler_module.as_deref() == Some(fantastic_core::HANDLER_MODULE)
        })
        .map(|l| l.key().0.clone())
}

/// Whether `id` is a canvas node (a `canvas.tools` agent).
fn is_canvas(h: &Host, id: &str) -> bool {
    h.kernel
        .agents
        .get(&fantastic_kernel::AgentId::from(id))
        .is_some_and(|a| a.value().record().handler_module.as_deref() == Some(crate::kern::backend::HANDLER_MODULE))
}

/// Every canvas: `(node, loader)` — each `canvas.tools` node under `canvas_page`,
/// and each canvas nested under a canvas (`new_canvas` on it), at any depth — with
/// its page loader (its `kernel_state` child). A flat peer made before nesting is
/// found the same way, so it loads as it always did.
pub fn canvases(h: &Host) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut parents = vec![CANVAS_PAGE_ID.to_string()];
    while let Some(parent) = parents.pop() {
        for a in h.kernel.agents.iter() {
            let r = a.value().record();
            if r.parent_id.as_deref() != Some(parent.as_str()) || r.handler_module.as_deref() != Some(crate::kern::backend::HANDLER_MODULE) {
                continue;
            }
            let node = a.key().0.clone();
            if let Some(loader) = loader_of(h, &node) {
                out.push((node.clone(), loader));
            }
            parents.push(node);
        }
    }
    out.sort();
    out
}

/// The node id a canvas named `name` takes: `canvas_<slug>`, the slug its letters
/// and digits (other characters `_`, the ends trimmed). A name with none is refused.
pub fn canvas_node_id(name: &str) -> Result<String, String> {
    let slug: String = name.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let slug = slug.trim_matches('_');
    if slug.is_empty() {
        return Err("new_canvas needs a name (letters or digits)".into());
    }
    Ok(format!("canvas_{slug}"))
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
    if !is_canvas(h, parent) {
        return Err(format!("new_canvas: `{parent}` is not a canvas"));
    }
    let node = canvas_node_id(name)?;
    if exists(h, &node) {
        return Err(format!("a canvas `{node}` already exists"));
    }
    let parent_loader = loader_of(h, parent).ok_or_else(|| format!("new_canvas: `{parent}` has no page loader"))?;
    let parent_root = meta(h, &parent_loader)
        .get("root")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("new_canvas: the page loader of `{parent}` has no root"))?;
    let loader = format!("{node}_loader");
    // `create_agent` boots each as it makes it
    ensure_canvas(h, parent, &node, &loader, &child_records_root(&parent_root, &node), false).await?;
    Ok(node)
}

/// A port free on this machine now (the OS picks it), for the first run that names none.
fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("no free port to pick: {e}"))?;
    l.local_addr().map(|a| a.port()).map_err(|e| format!("no free port to pick: {e}"))
}

/// What `ensure` serves: the canvas's page, on its port.
pub struct Urls {
    pub canvas: String,
    pub port: u16,
}

/// THE RUN FILE (owner 2026-10-16, for a local manager of fantastics): while this
/// process serves the canvas, `{pid, port, url}` — written once it serves,
/// removed at a clean exit. Under the project dir. A killed process leaves it
/// behind: the lock (`.fantastic/lock.yaml`, flocked while the kernel runs) says
/// whether it is live.
pub const RUN_FILE: &str = fantastic_home::RUN_FILE;

/// Write `dir`'s [`RUN_FILE`] for this process.
pub fn write_run_file(dir: &std::path::Path, urls: &Urls) -> Result<(), String> {
    crate::atomic::write_yaml(&dir.join(RUN_FILE), &json!({"pid": std::process::id(), "port": urls.port, "url": urls.canvas}))
}

/// Remove `dir`'s [`RUN_FILE`] at a clean exit (none written: nothing to remove).
pub fn remove_run_file(dir: &std::path::Path) {
    let _ = std::fs::remove_file(dir.join(RUN_FILE));
}

/// Create what is missing, grant, boot, mount. Returns the pages' URLs.
pub async fn ensure(h: &Host, port: Option<u16>) -> Result<Urls, String> {
    let recorded = meta(h, WEB_ID).get("port").and_then(Value::as_u64).map(|p| p as u16);
    let port = match (exists(h, WEB_ID), port) {
        // Repeating the recorded port changes nothing, so it is not a conflict.
        (true, Some(p)) if recorded == Some(p) => p,
        // `Host::boot` already set a new --port on the record before the
        // listener booted; a mismatch here means that write was refused.
        (true, Some(p)) => {
            return Err(format!(
                "--port {p}: the `web` leg still serves port {} (its record refused the new port)",
                recorded.map_or("?".into(), |r| r.to_string())
            ))
        }
        (true, None) => recorded.ok_or("the `web` record has no port")?,
        // THE FIRST RUN WITHOUT --port (owner 2026-10-11): a free port, picked once
        // and recorded — every later run reuses it, so the command is idempotent
        (false, None) => free_port()?,
        (false, Some(p)) => p,
    };
    let user = crate::env::HUMAN;
    // A hydrated host is ALREADY listening (`Host::boot` booted it): booting it
    // again restarts the listener and drops every page connected meanwhile
    // (measured). So `web` is booted only when it is created here.
    let fresh_host = !exists(h, WEB_ID);
    if fresh_host {
        create(h, json!({"parent_id": user, "id": WEB_ID, "handler_module": fantastic_web::HANDLER_MODULE, "port": port})).await?;
    }

    // THE CANVAS.
    if !exists(h, CANVAS_PAGE_ID) {
        create(h, json!({"parent_id": WEB_ID, "id": CANVAS_PAGE_ID, "handler_module": fantastic_canvas_page::HANDLER_MODULE,
            "verbs": {"read_stream": fantastic_canvas_page::READ_STREAM_DOC}})).await?;
    }
    let fresh_canvas_ws = !exists(h, CANVAS_WS_ID);
    if fresh_canvas_ws {
        create(h, json!({"parent_id": WEB_ID, "id": CANVAS_WS_ID, "handler_module": fantastic_web_ws::HANDLER_MODULE,
            "ingress_rule": "allow_all_ingress"})).await?;
    }
    // THE MAIN CANVAS: its node, its page records, its cells' backends.
    ensure_canvas(h, CANVAS_PAGE_ID, CANVAS_NODE_ID, CANVAS_LOADER_ID, ".fantastic/web", true).await?;
    // THE STATIC FILES: one read-only file leg over the project dir, for every
    // view. DENY-ALL per path (`allow: []`): its `allow` list — the record's,
    // written from above (`update_agent`) — exposes what the cells load. `web`
    // serves it, reading AS ITSELF.
    if !exists(h, STATIC_FILES_ID) {
        create(h, json!({"parent_id": CANVAS_NODE_ID, "id": STATIC_FILES_ID, "handler_module": "file_bridge.tools",
            "root": ".", "readonly": true, "allow": [], "ingress_rule": "allow_all_ingress"})).await?;
    }
    // `web` MOUNTS its legs by asking each for its routes, as itself
    merge_verbs(h, CANVAS_WS_ID, "callable_by", WEB_ID, json!(["get_routes"])).await?;
    merge_verbs(h, WEB_ID, "can_call", CANVAS_WS_ID, json!(["get_routes"])).await?;
    // `web` serves every `/<agent>/file/` route READING AS ITSELF: the page,
    // and the static files, both ends
    for served in [CANVAS_PAGE_ID, STATIC_FILES_ID] {
        merge_verbs(h, served, "callable_by", WEB_ID, json!(["read_stream"])).await?;
        merge_verbs(h, WEB_ID, "can_call", served, json!(["read_stream"])).await?;
    }
    // `user`: the leg's one reach (its verbs), and the page loader it reads.
    let gw_verbs = json!(fantastic_user::VERBS);
    let r = h.operator("kernel", json!({"type": "update_agent", "id": user, "page_loader": CANVAS_LOADER_ID})).await;
    if let Some(e) = err_of(&r) {
        return Err(format!("could not name `{CANVAS_LOADER_ID}` on `{user}`: {e}"));
    }
    merge_verbs(h, user, "callable_by", CANVAS_WS_ID, gw_verbs.clone()).await?;
    // the leg's can_call IS `user`, plus each connected page's agents
    // (`<guid>|*`, written by `kern/reach` when the page connects — `around`
    // judges by it); the record is rewritten whole
    let r = h.operator("kernel", json!({"type": "update_agent", "id": CANVAS_WS_ID,
        "can_call": {user: gw_verbs}})).await;
    if let Some(e) = err_of(&r) {
        return Err(format!("could not narrow `{CANVAS_WS_ID}` to `{user}`: {e}"));
    }

    let mut boots = vec![CANVAS_PAGE_ID, CANVAS_WS_ID, STATIC_FILES_ID];
    if fresh_host {
        boots.push(WEB_ID); // its boot mounts every child, then listens
    }
    for id in boots {
        let r = h.operator(id, json!({"type":"boot"})).await;
        if let Some(e) = err_of(&r) {
            return Err(format!("`{id}` did not boot: {e}"));
        }
    }
    // A leg created under a host that was already listening is mounted onto it.
    if fresh_canvas_ws && !fresh_host {
        let r = h.operator(WEB_ID, json!({"type":"mount","child_id": CANVAS_WS_ID})).await;
        if let Some(e) = err_of(&r) {
            return Err(format!("could not mount `{CANVAS_WS_ID}`: {e}"));
        }
    }
    // every canvas the project holds: its node and its loader
    for (node, loader) in canvases(h) {
        boot_canvas(h, &node, &loader).await?;
    }
    // GOD MODE, by grants: `user` and the canvas's AIs reach every agent, both edges
    // (`kern/reach`); the watch for agents added later starts after this (`vm.rs`)
    crate::kern::reach::grant_all(h).await?;
    Ok(Urls {
        canvas: format!("http://127.0.0.1:{port}/{CANVAS_PAGE_ID}/file/index.html"),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_canvas_takes_its_node_id_from_its_name_or_is_refused_without_one() {
        assert_eq!(canvas_node_id("research").unwrap(), "canvas_research");
        assert_eq!(canvas_node_id("  Research Team! ").unwrap(), "canvas_research_team");
        assert_eq!(canvas_node_id("a-b").unwrap(), "canvas_a_b");
        assert!(canvas_node_id("").unwrap_err().contains("needs a name"));
        assert!(canvas_node_id(" -- ").unwrap_err().contains("needs a name"));
    }

    #[test]
    fn a_child_canvas_records_nest_under_its_parents_records_dir() {
        // the main canvas's records are `.fantastic/web`: its child's sit inside them
        let under_main = child_records_root(".fantastic/web", "canvas_cabinet_a");
        assert_eq!(under_main, ".fantastic/web/canvas_cabinet_a");
        // a grandchild nests under its parent's, one level deeper
        assert_eq!(child_records_root(&under_main, "canvas_eq"), ".fantastic/web/canvas_cabinet_a/canvas_eq");
    }

    #[test]
    fn the_run_file_names_this_process_its_port_and_url_until_a_clean_exit() {
        let d = std::env::temp_dir().join(format!("fc-runfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let urls = Urls { canvas: "http://127.0.0.1:8123/canvas_page/file/index.html".into(), port: 8123 };
        write_run_file(&d, &urls).unwrap();
        let v = crate::atomic::read_yaml(&d.join(RUN_FILE)).unwrap();
        assert_eq!(v, json!({"pid": std::process::id(), "port": 8123, "url": urls.canvas}));
        remove_run_file(&d);
        assert!(!d.join(RUN_FILE).exists());
        remove_run_file(&d); // nothing left: still fine
        let _ = std::fs::remove_dir_all(&d);
    }
}
