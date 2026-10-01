//! THE CANVAS'S AGENTS, created by code on `fantastic canvas` (never by the
//! blueprint).
//!
//! ```text
//! <root>
//! └── web         web.tools       the HTTP host, loopback, `port` from --port
//!     ├── canvas_page  canvas_page.tools  THE CANVAS: its page, embedded in the binary
//!     └── canvas_ws    web_ws.tools       the canvas's socket leg
//!         ├── web_loader  kernel_state    the canvas's ws loader (.fantastic/web)
//!         ├── ai_cells    ai_cells.tools  spawns AI cells from the user's templates
//!         ├── activity    activity.tools  the open questions to the human, for the sidebar
//!         └── <backends>                  one per view the canvas spawns
//! ```
//!
//! THE CANVAS (design 33) replaced the browser View of the terminal shell
//! (owner, 2026-09-30): its page is served by `canvas_page` through `web`'s
//! `/file/` route; the page speaks through `canvas_ws`, which reaches EVERY
//! agent — god mode, by grants both sides (`kern/reach`).
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
/// The root verbs the canvas's leg names explicitly (a `*` never covers them).
const CANVAS_ROOT_VERBS: [&str; 3] = ["*", "create_agent", "delete_agent"];

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

async fn merge(h: &Host, id: &str, field: &str, add: &str) -> Result<(), String> {
    merge_verbs(h, id, field, add, json!("*")).await
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
    let root = h.root.clone();
    // A hydrated host is ALREADY listening (`Host::boot` booted it): booting it
    // again restarts the listener and drops every page connected meanwhile
    // (measured). So `web` is booted only when it is created here.
    let fresh_host = !exists(h, WEB_ID);
    if fresh_host {
        create(h, json!({"parent_id": root, "id": WEB_ID, "handler_module": fantastic_web::HANDLER_MODULE, "port": port})).await?;
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
    if !exists(h, CANVAS_LOADER_ID) {
        create(h, json!({"parent_id": CANVAS_WS_ID, "id": CANVAS_LOADER_ID, "handler_module": fantastic_core::HANDLER_MODULE,
            "root": ".fantastic/web", "watch": false})).await?;
    }
    // THE CANVAS'S AI CELLS: the helper that spawns them from the user's templates.
    if !exists(h, crate::kern::ai_cells::AI_CELLS_ID) {
        create(h, json!({"parent_id": CANVAS_WS_ID, "id": crate::kern::ai_cells::AI_CELLS_ID,
            "handler_module": crate::kern::ai_cells::HANDLER_MODULE})).await?;
    }
    // THE CANVAS'S ACTIVITY: the open questions to the human, for the sidebar.
    if !exists(h, crate::kern::activity::ACTIVITY_ID) {
        create(h, json!({"parent_id": CANVAS_WS_ID, "id": crate::kern::activity::ACTIVITY_ID,
            "handler_module": crate::kern::activity::HANDLER_MODULE})).await?;
    }
    merge(h, CANVAS_WS_ID, "can_call", CANVAS_LOADER_ID).await?;
    merge_verbs(h, CANVAS_WS_ID, "can_call", &root, json!(CANVAS_ROOT_VERBS)).await?;
    merge_verbs(h, &root, "callable_by", CANVAS_WS_ID, json!(CANVAS_ROOT_VERBS)).await?;

    let mut boots = vec![CANVAS_PAGE_ID, CANVAS_WS_ID, CANVAS_LOADER_ID, crate::kern::ai_cells::AI_CELLS_ID, crate::kern::activity::ACTIVITY_ID];
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
    // GOD MODE, by grants: the canvas leg reaches every agent, both edges (`kern/reach`).
    crate::kern::reach::start(h).await?;
    Ok(Urls {
        canvas: format!("http://127.0.0.1:{port}/{CANVAS_PAGE_ID}/file/index.html"),
    })
}
