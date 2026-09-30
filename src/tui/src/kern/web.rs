//! THE BROWSER VIEW'S AGENTS, created by code on `fantastic web` (never by the
//! blueprint), and the frame emitter.
//!
//! ```text
//! <root>
//! ├── ui          ui.tools        the ViewModel's face; frames are emitted here
//! └── web         web.tools       the HTTP host, loopback, `port` from --port
//!     ├── web_ui       web_ui.tools       the socket; every frame runs AS web_ui
//!     ├── ui_assets    file_bridge        the page, read-only, .fantastic/ui_assets
//!     ├── canvas_page  canvas_page.tools  THE CANVAS: its page, embedded in the binary
//!     └── canvas_ws    web_ws.tools       the canvas's socket leg
//!         ├── web_loader  kernel_state    the canvas's ws loader (.fantastic/web)
//!         ├── ai_cells    ai_cells.tools  spawns AI cells from the user's templates
//!         └── <backends>                  one per view the canvas spawns
//! ```
//!
//! Grants, both sides, merged never replaced: `web_ui.can_call {ui}` and
//! `ui.callable_by {web_ui}`. That is all the browser View may reach: it draws
//! frames and sends intents; it reads no agent itself.
//!
//! THE CANVAS sits beside the View under the same host (design 33): its page is
//! served by `canvas_page` through `web`'s `/file/` route; the page speaks
//! through `canvas_ws`, which may reach its loader and the root's kernel verbs
//! (it creates and deletes the backends its views front — as its own children),
//! and the root accepts it.
//!
//! `port` is CREATION-ONLY, like the `mcp` leg's flags: the first `fantastic
//! web` needs `--port N`, and later runs read it from `web`'s record.

use std::sync::{Arc, Mutex};

use ratatui::style::Color;
use serde_json::{json, Map, Value};
use tokio::sync::watch;

use crate::env::Paths;
use crate::kern::host::Host;
use crate::kern::ui_agent::UI_ID;
use crate::vm::Frame;

pub const WEB_ID: &str = "web";
pub const WEB_UI_ID: &str = "web_ui";
pub const ASSETS_ID: &str = "ui_assets";
pub const CANVAS_PAGE_ID: &str = "canvas_page";
pub const CANVAS_WS_ID: &str = "canvas_ws";
/// The canvas page addresses its loader by this id (`src/tui/web/canvas`).
pub const CANVAS_LOADER_ID: &str = "web_loader";
/// The root verbs the canvas's leg names explicitly (a `*` never covers them).
const CANVAS_ROOT_VERBS: [&str; 3] = ["*", "create_agent", "delete_agent"];
/// The page's home, inside the store so the file bridge's running-dir clamp
/// admits it. Rewritten on every start: it ships with the binary.
pub const ASSETS_DIR: &str = "ui_assets";
const INDEX_HTML: &str = include_str!("../../web/index.html");

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

/// What `ensure` serves: the browser View's page and the canvas's.
pub struct Urls {
    pub view: String,
    pub canvas: String,
}

/// Create what is missing, grant, boot, mount. Returns the pages' URLs.
pub async fn ensure(h: &Host, paths: &Paths, port: Option<u16>) -> Result<Urls, String> {
    let dir = paths.store.join(ASSETS_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(dir.join("index.html"), INDEX_HTML).map_err(|e| format!("{}: {e}", dir.display()))?;

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
        (false, None) => return Err("the first `fantastic web` needs --port N (no default)".into()),
        (false, Some(p)) => p,
    };
    let root = h.root.clone();
    if !exists(h, UI_ID) {
        create(h, json!({"parent_id": root, "id": UI_ID, "handler_module": crate::kern::ui_agent::HANDLER_MODULE})).await?;
    }
    // A hydrated host is ALREADY listening (`Host::boot` booted it): booting it
    // again restarts the listener and drops every page connected meanwhile
    // (measured). So `web` is booted only when it is created here.
    let fresh_host = !exists(h, WEB_ID);
    if fresh_host {
        create(h, json!({"parent_id": root, "id": WEB_ID, "handler_module": fantastic_web::HANDLER_MODULE, "port": port})).await?;
    }
    let fresh_leg = !exists(h, WEB_UI_ID);
    if fresh_leg {
        create(h, json!({"parent_id": WEB_ID, "id": WEB_UI_ID, "handler_module": fantastic_web_ui::HANDLER_MODULE,
            "ingress_rule": "allow_all_ingress"})).await?;
    }
    if !exists(h, ASSETS_ID) {
        create(h, json!({"parent_id": WEB_ID, "id": ASSETS_ID, "handler_module": "file_bridge.tools",
            "root": format!(".fantastic/{ASSETS_DIR}"), "readonly": true, "allow": ["index.html"],
            "ingress_rule": "allow_all_ingress"})).await?;
    }
    merge(h, WEB_UI_ID, "can_call", UI_ID).await?;
    merge(h, UI_ID, "callable_by", WEB_UI_ID).await?;

    // THE CANVAS, beside the View.
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
    merge(h, CANVAS_WS_ID, "can_call", CANVAS_LOADER_ID).await?;
    merge_verbs(h, CANVAS_WS_ID, "can_call", &root, json!(CANVAS_ROOT_VERBS)).await?;
    merge_verbs(h, &root, "callable_by", CANVAS_WS_ID, json!(CANVAS_ROOT_VERBS)).await?;

    let mut boots = vec![UI_ID, WEB_UI_ID, ASSETS_ID, CANVAS_PAGE_ID, CANVAS_WS_ID, CANVAS_LOADER_ID, crate::kern::ai_cells::AI_CELLS_ID];
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
    // (`web` orders its routes literal-first, so `/web_ui/ws` wins over the
    // canvas leg's `/{host_id}/ws` whatever the mount order.)
    for (fresh, leg) in [(fresh_leg, WEB_UI_ID), (fresh_canvas_ws, CANVAS_WS_ID)] {
        if fresh && !fresh_host {
            let r = h.operator(WEB_ID, json!({"type":"mount","child_id": leg})).await;
            if let Some(e) = err_of(&r) {
                return Err(format!("could not mount `{leg}`: {e}"));
            }
        }
    }
    Ok(Urls {
        view: format!("http://127.0.0.1:{port}/{ASSETS_ID}/file/index.html"),
        canvas: format!("http://127.0.0.1:{port}/{CANVAS_PAGE_ID}/file/index.html"),
    })
}

// ── frames on the wire ───────────────────────────────────────────────────────

/// The last frame emitted, and its emit counter — what `state` answers, so a
/// resync lands exactly where the diffs continue.
static LAST: Mutex<Option<(u64, Arc<Frame>)>> = Mutex::new(None);

pub fn color(c: Color) -> Value {
    match c {
        Color::Reset => Value::Null,
        Color::Rgb(r, g, b) => json!(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Indexed(i) => json!(format!("i{i}")),
        named => json!(format!("{named:?}").to_lowercase()),
    }
}

/// One row as runs of same-styled text.
pub fn row(f: &Frame, y: u16) -> Value {
    let mut runs: Vec<Value> = Vec::new();
    let mut cur: Option<(Value, Value, u16, String)> = None;
    for x in 0..f.buf.area.width {
        let Some(c) = f.buf.cell((x, y)) else { continue };
        let (fg, bg, m) = (color(c.fg), color(c.bg), c.modifier.bits());
        let sym = c.symbol();
        match cur.as_mut() {
            Some((cf, cb, cm, t)) if *cf == fg && *cb == bg && *cm == m => t.push_str(sym),
            _ => {
                if let Some(r) = cur.take() {
                    runs.push(run(r));
                }
                cur = Some((fg, bg, m, sym.to_string()));
            }
        }
    }
    if let Some(r) = cur {
        runs.push(run(r));
    }
    Value::Array(runs)
}

fn run((fg, bg, m, t): (Value, Value, u16, String)) -> Value {
    let mut o = Map::new();
    o.insert("t".into(), json!(t));
    if !fg.is_null() {
        o.insert("fg".into(), fg);
    }
    if !bg.is_null() {
        o.insert("bg".into(), bg);
    }
    if m != 0 {
        o.insert("m".into(), json!(m));
    }
    Value::Object(o)
}

fn encode(seq: u64, f: &Frame, rows: impl Iterator<Item = u16>, full: bool) -> Value {
    let rows: Map<String, Value> = rows.map(|y| (y.to_string(), row(f, y))).collect();
    json!({
        "type": "frame", "seq": seq, "full": full,
        "w": f.buf.area.width, "h": f.buf.area.height,
        "rows": rows,
        "cursor": f.cursor.map(|(x, y)| json!([x, y])),
    })
}

/// `ui.state`: the last emitted frame, whole.
pub fn state() -> Option<Value> {
    let last = LAST.lock().expect("frame lock");
    last.as_ref().map(|(seq, f)| encode(*seq, f, 0..f.buf.area.height, true))
}

/// Emit every new frame on `ui` as the rows that changed since the last one.
pub fn spawn_emitter(h: Arc<Host>, mut frames: watch::Receiver<Option<Arc<Frame>>>) {
    tokio::spawn(async move {
        let ui = fantastic_kernel::AgentId::from(UI_ID);
        let mut prev: Option<Arc<Frame>> = None;
        let mut seq = 0u64;
        // The frame already on screen counts: a page that asks for `state`
        // before anything changes must get it, not "no frame yet" (measured).
        frames.mark_changed();
        while frames.changed().await.is_ok() {
            let Some(f) = frames.borrow_and_update().clone() else { continue };
            let full = prev.as_ref().is_none_or(|p| p.buf.area != f.buf.area);
            let changed: Vec<u16> = (0..f.buf.area.height)
                .filter(|y| full || prev.as_ref().is_some_and(|p| row_differs(p, &f, *y)))
                .collect();
            let cursor_moved = prev.as_ref().is_none_or(|p| p.cursor != f.cursor);
            if !full && changed.is_empty() && !cursor_moved {
                continue;
            }
            seq += 1;
            let payload = encode(seq, &f, changed.into_iter(), full);
            *LAST.lock().expect("frame lock") = Some((seq, Arc::clone(&f)));
            prev = Some(f);
            h.kernel.emit(&ui, payload).await;
        }
    });
}

fn row_differs(a: &Frame, b: &Frame, y: u16) -> bool {
    (0..b.buf.area.width).any(|x| a.buf.cell((x, y)) != b.buf.cell((x, y)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};

    #[test]
    fn a_row_is_runs_of_same_styled_text() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
        buf.set_string(0, 0, "ab", Style::default().fg(Color::Rgb(1, 2, 3)));
        buf.set_string(2, 0, "cd", Style::default().add_modifier(Modifier::BOLD));
        let f = Frame { buf, cursor: None, mouse: false };
        assert_eq!(
            row(&f, 0),
            json!([{"t":"ab","fg":"#010203"},{"t":"cd","m":Modifier::BOLD.bits()},{"t":"  "}])
        );
    }
}
