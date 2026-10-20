//! `user` — THE ONE HUMAN, as an agent (`user.tools`; owner, 2026-10-03/04: one
//! user, the root of everything that is the human's). Every call the terminal
//! and the canvas make goes OUT THROUGH IT, so it is the sender: its grants are
//! checked and replies route back to it.
//!
//! Verbs:
//! - `forward {target, payload, timeout?}` — `user` sends `payload` to
//!   `target`; the reply comes back verbatim. NO DEFAULT TIMEOUT: the call
//!   lasts as long as the target takes (an AI turn may run for minutes); a
//!   `timeout` given is honoured. No secret marks — values travel and are
//!   recorded as they are. Who may call it is `user`'s `callable_by`, nothing
//!   else. A page loader serves record verbs inside its caller's subtree; the
//!   canvas's loader sits under `user`.
//! - `around {canvas?, view?, x, y, radius}` — the cells of a page view
//!   (default: the root view, [`ROOT_VIEW`]) of a canvas (its node id; default
//!   the main canvas, `page_loader`) within `radius` of the point, nearest first,
//!   from the page RECORDS: `{view, members: [{id, kind, title, rect,
//!   distance, backend_id?}]}`. Judged by the CALLER's own page grants: the
//!   shell sees everything; any other caller — the canvas leg (`*|*`), a
//!   cell's AI — by its own `can_call` (`<tree guid>|<id>`).
//! - `path {rescan?}` — THE HUMAN'S PROGRAMS: `{dirs, executables: {name:
//!   full path}}` — the PATH directories (this process's, merged with the
//!   login shell's) and every executable on them, the first on PATH winning.
//!   Scanned when `user` boots and kept in RAM; `rescan: true` scans again.
//!   What a name MEANS (a coding agent, an editor…) is the caller's to decide.
//! - `reflect` — this agent, its verbs, and (when it can) its `can_call`.
//! - every other verb is the lib's `user_probe` (the human as a test fixture:
//!   `inquiry`, `inquiries`), unchanged — but its `send` (any payload to any
//!   agent, as the human) is refused: every agent may `send` to `user`.

pub mod grants;
pub mod inquiries;
pub mod path;
mod sys;

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Blob, Item, Kernel, Status, Step};
use fantastic_scenario_probes::UserProbe;
use serde_json::{json, Map, Value};

/// The handler.
pub const HANDLER_MODULE: &str = "user.tools";

/// The verbs this bundle answers itself (the rest are the probe's).
pub const VERBS: [&str; 7] = ["forward", "around", "path", "reflect", "inquiry", "inquiries", "answer_inquiry"];


/// A view's children that are not cells: its background (drawn behind them),
/// its theme layouts, a double click's selector, the canvas theme agent.
const NOT_CELLS: [&str; 4] = ["background.ts", "layout.ts", "selector.ts", "backgrounds.ts"];

/// The theme `around` reads rects in by default: the free 2D canvas.
const AROUND_THEME: &str = "canvas";

/// The page's root view: `around`'s default.
pub const ROOT_VIEW: &str = "view_root";

/// A cell's kind, as `place` names it, from its handler.
fn kind_of(hm: &str) -> &str {
    match hm {
        "html_agent.ts" => "html",
        "gl_agent.ts" => "gl",
        "terminal_view.ts" => "terminal",
        "ai_view.ts" => "chat",
        "view.ts" => "view",
        "setup_view.ts" => "setup",
        "stream_view.ts" => "stream",
        "reflect_view.ts" => "reflect",
        // sidebar_view.ts is the cell's name before the file tree (#106): saved layouts still hold it, and it opens as the tree
        "sidebar_view.ts" => "sidebar",
        "files_view.ts" => "files",
        "preview_view.ts" => "preview",
        "canvases_view.ts" => "canvases",
        other => other,
    }
}

fn refuse(reason: &str, msg: impl Into<String>) -> Value {
    json!({"error": msg.into(), "reason": reason})
}

fn s<'a>(p: &'a Value, k: &str) -> &'a str {
    p.get(k).and_then(Value::as_str).unwrap_or("")
}

/// The `user` bundle: its own verbs, then the probe's.
#[derive(Default)]
pub struct UserBundle {
    probe: UserProbe,
    /// `path`'s scan, made at boot (RAM only — a stale path is never stored)
    programs: Arc<tokio::sync::Mutex<Option<Value>>>,
    /// the questions agents asked the person, open until answered
    inquiries: inquiries::Inquiries,
}

impl UserBundle {
    fn page_loader(kernel: &Kernel, gw: &AgentId) -> Option<String> {
        kernel
            .agents
            .get(gw)
            .and_then(|a| a.value().record().meta.get("page_loader").and_then(Value::as_str).map(str::to_string))
    }

    /// A canvas node's page loader: its `kernel_state` child.
    fn loader_of(kernel: &Kernel, canvas: &str) -> Option<String> {
        kernel
            .agents
            .iter()
            .find(|a| {
                let r = a.value().record();
                r.parent_id.as_deref() == Some(canvas) && r.handler_module.as_deref() == Some("kernel_state.tools")
            })
            .map(|a| a.key().0.clone())
    }

    /// One item, sent from inside this handler: its sender is `user`.
    async fn send_as(kernel: &Arc<Kernel>, item: Item) -> (Value, Option<Blob>) {
        let k = Arc::clone(kernel);
        let mut out = k.send(vec![item]).await;
        let o = out.remove(0);
        let result = match o.status {
            Status::TimedOut if o.result.is_null() => refuse("timed_out", "user.forward: timeout"),
            _ => o.result,
        };
        (result, o.body)
    }

    /// The page tree as the loader holds it, and its guid.
    async fn tree(kernel: &Arc<Kernel>, loader: &str) -> (Vec<Value>, String) {
        let (r, _) = Self::send_as(kernel, Item::new(AgentId::from(loader), json!({"type": "load_tree"}))).await;
        let recs = r.get("records").and_then(Value::as_array).cloned().unwrap_or_default();
        let guid = recs
            .iter()
            .find(|x| x.get("parent_id").and_then(Value::as_str) == Some(loader))
            .and_then(|x| x.get("guid").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        (recs, guid)
    }

    /// The ancestors of a record (nearest first), from its `parent`.
    fn ancestors(recs: &[Value], loader: &str, parent: Option<&str>) -> Vec<String> {
        let mut up = Vec::new();
        let mut cur = parent.map(str::to_string);
        while let Some(p) = cur.filter(|p| p != loader && !up.contains(p)) {
            cur = recs
                .iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some(p.as_str()))
                .and_then(|r| r.get("parent_id").and_then(Value::as_str))
                .map(str::to_string);
            up.push(p);
        }
        up
    }

    /// `forward` — `user` calls `target`.
    async fn forward(p: &Value, body: Option<Blob>, kernel: &Arc<Kernel>) -> (Value, Option<Blob>) {
        let target = s(p, "target");
        let Some(inner) = p.get("payload").filter(|v| v.is_object()).cloned() else {
            return (refuse("bad_arg", "user.forward: target (str) + payload (dict) required"), None);
        };
        if target.is_empty() {
            return (refuse("bad_arg", "user.forward: target (str) + payload (dict) required"), None);
        }
        let timeout = match p.get("timeout") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_f64() {
                Some(t) => Some(t),
                None => return (refuse("bad_arg", "user.forward: timeout must be a number"), None),
            },
        };
        let mut item = Item::new(AgentId::from(target), inner);
        item.timeout = timeout;
        if let Some(b) = body {
            // the body's slot is under this call's `payload`: one step less on the far item
            let mut path = b.path;
            if path.first() == Some(&Step::Key("payload".into())) {
                path.remove(0);
            }
            item = item.with_binary(path, b.bytes);
        }
        // the forward's secret marks under `payload` mark the onward send the same way
        let marks: Vec<String> = fantastic_kernel::current_secret()
            .into_iter()
            .filter_map(|m| m.strip_prefix("payload.").map(str::to_string))
            .collect();
        if !marks.is_empty() {
            item = item.with_secret(marks);
        }
        Self::send_as(kernel, item).await
    }

    /// `around {view?, x, y, radius}` from the page records.
    async fn around(me: &AgentId, p: &Value, kernel: &Arc<Kernel>) -> Value {
        let loader = match p.get("canvas").and_then(Value::as_str).filter(|c| !c.is_empty()) {
            Some(c) => match Self::loader_of(kernel, c) {
                Some(l) => l,
                None => return refuse("not_found", format!("user.around: no canvas {c:?} (a canvas node with a page loader)")),
            },
            None => match Self::page_loader(kernel, me) {
                Some(l) => l,
                None => return refuse("not_found", "user.around: this record names no page_loader"),
            },
        };
        let num = |k: &str| p.get(k).and_then(Value::as_f64).filter(|v| v.is_finite());
        let (Some(x), Some(y), Some(radius)) = (num("x"), num("y"), num("radius")) else {
            return refuse("bad_arg", "user.around needs x, y and radius as numbers (radius > 0)");
        };
        if radius <= 0.0 {
            return refuse("bad_arg", "user.around needs x, y and radius as numbers (radius > 0)");
        }
        // whose page grants judge it: `None` = the shell, sees everything; any
        // other caller by its own page grants (the canvas leg holds `*|*`)
        let judge = fantastic_kernel::current_origin();
        let (recs, guid) = Self::tree(kernel, &loader).await;
        let view = p.get("view").and_then(Value::as_str).filter(|c| !c.is_empty()).unwrap_or(ROOT_VIEW).to_string();
        if !recs.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(view.as_str())) {
            return refuse("not_found", format!("user.around: no view {view:?} here"));
        }
        let up = Self::ancestors(&recs, &loader, Some(&view));
        // a cell's rect is not on its record but in the view's layout for a theme
        // (`layout_<theme>`, `rects: {<cell id>: {x, y, width, height}}`)
        let theme = p.get("theme").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or(AROUND_THEME);
        let layout = format!("layout_{theme}");
        let rects = recs
            .iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(layout.as_str()) && r.get("parent_id").and_then(Value::as_str) == Some(view.as_str()))
            .and_then(|r| r.get("rects").and_then(Value::as_object).cloned())
            .unwrap_or_default();
        let sees = |id: &str| match &judge {
            None => true,
            Some(u) => grants::sees(&grants::page_verbs(&grants::can_call(kernel, u), &guid, id, &up)),
        };
        let mut members: Vec<(f64, Value)> = recs
            .iter()
            .filter(|r| r.get("parent_id").and_then(Value::as_str) == Some(view.as_str()))
            .filter(|r| !r.get("handler_module").and_then(Value::as_str).is_some_and(|m| NOT_CELLS.contains(&m)))
            .filter_map(|r| {
                let id = r.get("id").and_then(Value::as_str)?;
                let rect = rects.get(id)?;
                let n = |k: &str| rect.get(k).and_then(Value::as_f64);
                let (rx, ry, w, h) = (n("x")?, n("y")?, n("width")?, n("height")?);
                if !sees(id) {
                    return None;
                }
                // to the rect's nearest edge; 0 inside
                let dx = (rx - x).max(0.0).max(x - (rx + w));
                let dy = (ry - y).max(0.0).max(y - (ry + h));
                let d = (dx * dx + dy * dy).sqrt();
                (d <= radius).then(|| {
                    let hm = r.get("handler_module").and_then(Value::as_str).unwrap_or("");
                    let mut m = json!({
                        "id": id,
                        "kind": kind_of(hm),
                        "title": r.get("display_name").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or(id),
                        "rect": {"x": rx, "y": ry, "w": w, "h": h},
                        "distance": (d * 10.0).round() / 10.0,
                    });
                    if let Some(b) = r.get("backend_id").and_then(Value::as_str) {
                        m["backend_id"] = json!(b);
                    }
                    (d, m)
                })
            })
            .collect();
        members.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        json!({"view": view, "members": members.into_iter().map(|(_, m)| m).collect::<Vec<_>>()})
    }

    /// `path {rescan?}` — the scan, made once (at boot) and kept.
    async fn path(&self, p: &Value) -> Value {
        let mut held = self.programs.lock().await;
        if held.is_none() || p.get("rescan").and_then(Value::as_bool) == Some(true) {
            *held = Some(path::scan().await);
        }
        held.clone().unwrap_or(Value::Null)
    }

    /// This bundle's own verbs; `None` = not one of them (the probe's).
    async fn run(&self, me: &AgentId, p: &Value, body: Option<Blob>, kernel: &Arc<Kernel>) -> Option<(Reply, Option<Blob>)> {
        let reply = match s(p, "type") {
            "path" => self.path(p).await,
            "boot" => {
                // the scan starts with `user`, off the boot's path
                let held = Arc::clone(&self.programs);
                tokio::spawn(async move {
                    let mut h = held.lock().await;
                    if h.is_none() {
                        *h = Some(path::scan().await);
                    }
                });
                return None;
            }
            "forward" => {
                let (r, b) = Self::forward(p, body, kernel).await;
                return Some((Some(r), b));
            }
            "around" => Self::around(me, p, kernel).await,
            // THE PERSON'S INQUIRIES: held, listed, answered (`inquiries.rs`)
            "inquiry" => self.inquiries.ask(p),
            "inquiries" => self.inquiries.list(p),
            "answer_inquiry" => self.inquiries.answer(me, p, kernel).await,
            _ => return None,
        };
        Some((Some(reply), None))
    }
}

#[async_trait]
impl Bundle for UserBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    /// HOW TO CALL it — the probe's verbs, then its own; served by the
    /// kernel's `docs` verb, never on a reflect.
    fn verb_docs(&self) -> Value {
        let mut v = match self.probe.verb_docs() {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        v.remove("send");
        v.insert("forward".into(), json!("args: target:str (req), payload:dict (req), timeout:num? (none by default: the call lasts as long as the target takes) — `user` sends payload to target; the reply verbatim."));
        v.insert("inquiry".into(), json!("args: text:str (req), kind:str? (text|single|multi|confirm, default text), options:list? (required for single/multi), request_id:str? — ACCEPTED at once ({accepted, re}) and held for the person; the answer arrives LATER as `[reply to <re> · user inquiry] <answer>…</answer>` on your thread. Nothing suspends: keep working, or await."));
        v.insert("inquiries".into(), json!("args: asker:str? — {inquiries: [{re, asker, text, kind, options, request_id, ts}]}: the open questions (one asker's, when named)."));
        v.insert("answer_inquiry".into(), json!("args: re:int (req — the inquiry's pid), answer: str | list | bool (req) — the person's answer, sent to the asker under the inquiry's pid; then closed."));
        v.insert("path".into(), json!("args: rescan:bool? — {dirs: [PATH dirs], executables: {name: full path}}: every program on the human's PATH (this process's merged with the login shell's), the first on PATH winning; scanned at boot, `rescan: true` scans again. The caller decides what a name means."));
        v.insert("around".into(), json!("args: canvas:str? (a canvas node id — default the main canvas), view:str? (default view_root), theme:str? (whose layout's rects — default canvas), x:num, y:num, radius:num — {view, members: [{id, kind, title, rect:{x,y,w,h}, distance, backend_id?}]}: the cells of a page view within radius, nearest first, from the page records; judged by YOUR page grants."));
        Value::Object(v)
    }

    async fn handle(&self, agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        if let Some((r, _)) = self.run(agent_id, payload, None, kernel).await {
            return Ok(r);
        }
        // the probe's `send` (any payload to any agent, AS the human) is not
        // the human's: every agent holds `send` on `user` for its messages
        if s(payload, "type") == "send" {
            return Ok(Some(refuse("unhandled", "user: `send` is not a verb of the human — use `forward` (the human's own calls)")));
        }
        let mut r = self.probe.handle(agent_id, payload, kernel).await?;
        if s(payload, "type") == "reflect" {
            if let Some(Value::Object(m)) = r.as_mut() {
                m.insert("handler_module".into(), json!(HANDLER_MODULE));
            }
        }
        Ok(r)
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        // every call lands here (the kernel's binary path)
        if let Some(out) = self.run(agent_id, &header, body, kernel).await {
            return Ok(out);
        }
        Ok((self.handle(agent_id, &header, kernel).await?, None))
    }

    fn validate_meta(&self, meta: &Map<String, Value>, record: &Map<String, Value>) -> Option<String> {
        match meta.get("page_loader") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if !s.is_empty() => {}
            Some(_) => return Some("page_loader must be the page loader's agent id".into()),
        }
        self.probe.validate_meta(meta, record)
    }
}
