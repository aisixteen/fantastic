//! THE CANVAS'S AGENTS, created by code on `fantastic canvas` (never by the
//! blueprint).
//!
//! ```text
//! user
//! └── web                 web.tools          the HTTP host, loopback, `port` from --port
//!     ├── canvas_ws       web_ws.tools       the canvas's socket leg
//!     └── canvas_page     canvas_page.tools  THE CANVAS: its page, embedded in the binary
//!         └── canvas_root canvas.tools       the canvas's host node: creates /
//!             │                              deletes its own children — the
//!             │                              cells' backends (AIs, exec agents)
//!             ├── static_files  file_bridge  THE files every view's cells load:
//!             │                              GET /static_files/file/<path>
//!             └── web_loader  kernel_state   the page records (.fantastic/web)
//!
//! No host id here is a page id: the page's records (`canvas`, `view_root`, …)
//! resolve against the host tree in the page loader.
//! ```
//!
//! THE LEG REACHES ONLY `user` (owner, 2026-10-03/04): a tab's every host call
//! is `user.forward` (with its token when `user` carries a `token_sha256`), and
//! `user` makes it — the shell's own identity, god mode by grants
//! (`kern/reach`).
//!
//! THE CANVAS (design 33) replaced the browser View of the terminal shell
//! (owner, 2026-09-30): its page is served by `canvas_page` through `web`'s
//! `/file/` route; the page speaks through `canvas_ws` to `user`.
//!
//! `port` is CREATION-ONLY, like the `mcp` leg's flags: the first `fantastic
//! canvas` needs `--port N`, and later runs read it from `web`'s record.

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

/// What `ensure` serves: the canvas's page.
pub struct Urls {
    pub canvas: String,
}

/// Create what is missing, grant, boot, mount. Returns the pages' URLs.
pub async fn ensure(h: &Host, port: Option<u16>) -> Result<Urls, String> {
    let recorded = meta(h, WEB_ID).get("port").and_then(Value::as_u64).map(|p| p as u16);
    let port = match (exists(h, WEB_ID), port) {
        // Repeating the recorded port changes nothing, so it is not a conflict.
        (true, Some(p)) if recorded == Some(p) => p,
        (true, Some(p)) => {
            return Err(format!(
                "--port is creation-only: the `web` leg already serves port {} (its record); --port {p} would change it — use update_agent",
                recorded.map_or("?".into(), |r| r.to_string())
            ))
        }
        (true, None) => recorded.ok_or("the `web` record has no port")?,
        (false, None) => return Err("the first `fantastic canvas` needs --port N (no default)".into()),
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
    // THE CANVAS'S HOST NODE: its page records, its files, its cells' backends.
    if !exists(h, CANVAS_NODE_ID) {
        create(h, json!({"parent_id": CANVAS_PAGE_ID, "id": CANVAS_NODE_ID,
            "handler_module": crate::kern::backend::HANDLER_MODULE})).await?;
    }
    if !exists(h, CANVAS_LOADER_ID) {
        create(h, json!({"parent_id": CANVAS_NODE_ID, "id": CANVAS_LOADER_ID, "handler_module": fantastic_core::HANDLER_MODULE,
            "root": ".fantastic/web", "watch": false})).await?;
    }
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
    merge_verbs(h, user, "can_call", CANVAS_LOADER_ID, json!(["*"])).await?;
    merge_verbs(h, CANVAS_LOADER_ID, "callable_by", user, json!(["*"])).await?;
    // the leg's can_call IS `user` plus every page agent (`around` judges by it);
    // the record is rewritten whole
    let r = h.operator("kernel", json!({"type": "update_agent", "id": CANVAS_WS_ID,
        "can_call": {user: gw_verbs, "*|*": ["*"]}})).await;
    if let Some(e) = err_of(&r) {
        return Err(format!("could not narrow `{CANVAS_WS_ID}` to `{user}`: {e}"));
    }
    // the shell's own page reach: every page agent of every tree
    merge_verbs(h, user, "can_call", "*|*", json!(["*"])).await?;

    let mut boots = vec![CANVAS_PAGE_ID, CANVAS_WS_ID, CANVAS_NODE_ID, CANVAS_LOADER_ID, STATIC_FILES_ID];
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
    // GOD MODE, by grants: `user` and the canvas's AIs reach every agent, both edges (`kern/reach`).
    crate::kern::reach::start(h).await?;
    Ok(Urls {
        canvas: format!("http://127.0.0.1:{port}/{CANVAS_PAGE_ID}/file/index.html"),
    })
}
