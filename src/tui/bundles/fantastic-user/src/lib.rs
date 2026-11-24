//! `user` — THE ONE HUMAN, as an agent (`user.tools`; owner, 2026-10-03/04: one
//! user, the root of everything that is the human's). The shell itself is the
//! operator; `user` is the person's place in the tree, the parent of the user
//! space and the system helpers.
//!
//! Its verbs are declared in [`verbs`] (`reflect` renders them), its record in
//! [`record`]: `around` (the cells of a page view near a point, from the page records,
//! shown to the shell only while page lines are parked), `path` (the human's programs, scanned at
//! boot), `export_subtree` / `import_subtree` (a subtree as a blueprint folder and back,
//! `export.rs` / `import.rs`), and the person's inquiries (the kernel's `inquiry` module). A
//! page loader serves record verbs inside its caller's subtree; the canvas's loader sits
//! under `user`.

pub mod export;
pub mod import;
pub mod path;
pub mod record;
mod sys;
pub mod verbs;

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Blob, Item, Kernel, Status};
use serde_json::{json, Value};

/// The handler.
pub const HANDLER_MODULE: &str = "user.tools";

/// What the bundle is for: its one short man text; how to call it is rendered from its
/// declared verbs (`docs`, `reflect`).
const MAN: &str = "The one human, as an agent: the parent of the user space and the system helpers. It holds the questions agents ask the person, answers a page's `around` queries, and keeps the human's program list.";

/// A view's children that are not cells: its background (drawn behind them),
/// its layouts, a double click's selector, the backgrounds agent.
const NOT_CELLS: [&str; 4] = [
    "background.ts",
    "layout.ts",
    "selector.ts",
    "backgrounds.ts",
];

/// The layout `around` reads rects from: the view's free 2D canvas.
const AROUND_LAYOUT: &str = "layout_canvas";

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

/// The `user` bundle.
#[derive(Default)]
pub struct UserBundle {
    /// `path`'s scan, made at boot (RAM only — a stale path is never stored)
    programs: Arc<tokio::sync::Mutex<Option<Value>>>,
}

impl UserBundle {
    fn page_loader(kernel: &Kernel, gw: &AgentId) -> Option<String> {
        kernel.resolve_target(gw.as_str()).and_then(|a| {
            a
                .record()
                .meta
                .get("page_loader")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    }

    /// A canvas node's page loader: its `kernel_state` child.
    fn loader_of(kernel: &Kernel, canvas: &str) -> Option<String> {
        kernel.agent_ids().into_iter().filter_map(|aid| kernel.resolve_target(aid.as_str()))
            .find(|a| {
                let r = a.record();
                r.parent_id.as_deref() == Some(canvas)
                    && r.handler_module.as_deref() == Some("kernel_state.tools")
            })
            .map(|a| a.id.0.clone())
    }

    /// One item, sent from inside this handler: its sender is `user`.
    async fn send_as(kernel: &Arc<Kernel>, item: Item) -> (Value, Option<Blob>) {
        let k = Arc::clone(kernel);
        let mut out = k.send(vec![item]).await;
        let o = out.remove(0);
        let result = match o.status {
            Status::TimedOut if o.result.is_null() => refuse("timed_out", "user: timeout"),
            _ => o.result,
        };
        (result, o.body)
    }

    /// The page tree as the loader holds it.
    async fn tree(kernel: &Arc<Kernel>, loader: &str) -> Vec<Value> {
        let (r, _) = Self::send_as(
            kernel,
            Item::new(AgentId::from(loader), json!({"type": "load_tree"})),
        )
        .await;
        r.get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// `around {view?, x, y, radius}` from the page records.
    async fn around(me: &AgentId, a: verbs::Around, kernel: &Arc<Kernel>) -> Value {
        let loader = match a.canvas.as_deref() {
            Some(c) => match Self::loader_of(kernel, c) {
                Some(l) => l,
                None => {
                    return refuse(
                        "not_found",
                        format!("user.around: no canvas {c:?} (a canvas node with a page loader)"),
                    )
                }
            },
            None => match Self::page_loader(kernel, me) {
                Some(l) => l,
                None => {
                    return refuse("not_found", "user.around: this record names no page_loader")
                }
            },
        };
        let (x, y, radius) = (a.x, a.y, a.radius);
        // page lines are parked with the peers (grant model v9, Q5): only the
        // shell (no agent origin) sees page agents; any agent caller sees none
        let shell = fantastic_kernel::current_origin().is_none();
        let recs = Self::tree(kernel, &loader).await;
        let view = a.view;
        if !recs
            .iter()
            .any(|r| r.get("id").and_then(Value::as_str) == Some(view.as_str()))
        {
            return refuse("not_found", format!("user.around: no view {view:?} here"));
        }
        // a cell's rect is not on its record but in the view's layout
        // (`layout_canvas`, `rects: {<cell id>: {x, y, width, height}}`)
        let rects = recs
            .iter()
            .find(|r| {
                r.get("id").and_then(Value::as_str) == Some(AROUND_LAYOUT)
                    && r.get("parent_id").and_then(Value::as_str) == Some(view.as_str())
            })
            .and_then(|r| r.get("rects").and_then(Value::as_object).cloned())
            .unwrap_or_default();
        let mut members: Vec<(f64, Value)> = recs
            .iter()
            .filter(|r| r.get("parent_id").and_then(Value::as_str) == Some(view.as_str()))
            .filter(|r| !r.get("handler_module").and_then(Value::as_str).is_some_and(|m| NOT_CELLS.contains(&m)))
            .filter_map(|r| {
                let id = r.get("id").and_then(Value::as_str)?;
                let rect = rects.get(id)?;
                let n = |k: &str| rect.get(k).and_then(Value::as_f64);
                let (rx, ry, w, h) = (n("x")?, n("y")?, n("width")?, n("height")?);
                if !shell {
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
    async fn path(&self, a: &verbs::Path) -> Value {
        let mut held = self.programs.lock().await;
        if held.is_none() || a.rescan == Some(true) {
            *held = Some(path::scan().await);
        }
        held.clone().unwrap_or(Value::Null)
    }

    fn reflect(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        json!({
            "id": agent_id.as_str(),
            "handler_module": HANDLER_MODULE,
            "inquiries": fantastic_kernel::inquiry::open_count(kernel, agent_id),
        })
    }
}

#[async_trait]
impl Handler for UserBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let reply = match call {
            verbs::Call::Boot(_) => {
                // the scan starts with `user`, off the boot's path
                let held = Arc::clone(&self.programs);
                tokio::spawn(async move {
                    let mut h = held.lock().await;
                    if h.is_none() {
                        *h = Some(path::scan().await);
                    }
                });
                return Ok(None);
            }
            verbs::Call::Reflect(_) => self.reflect(agent_id, kernel),
            verbs::Call::Path(a) => self.path(&a).await,
            verbs::Call::Around(a) => Self::around(agent_id, a, kernel).await,
            // A SUBTREE AS A BLUEPRINT, outside references as ports (`export.rs`)
            verbs::Call::ExportSubtree(a) => {
                fantastic_core::export::export_subtree(
                    kernel,
                    &a.agent_id,
                    &a.path,
                    "user.export_subtree",
                )
                .await
            }
            // A BLUEPRINT AS A LIVE SUBTREE: fresh ids, ports bound, approved (`import.rs`)
            verbs::Call::ImportSubtree(a) => {
                import::import_subtree(kernel, a).await
            }
            // THE PERSON'S INQUIRIES: held, listed, answered (the kernel's `inquiry` module)
            verbs::Call::Inquiry(a) => fantastic_kernel::inquiry::ask(agent_id, a, kernel).await,
            verbs::Call::Inquiries(a) => fantastic_kernel::inquiry::list(agent_id, &a, kernel),
            verbs::Call::AnswerInquiry(a) => fantastic_kernel::inquiry::answer(agent_id, a, kernel).await,
        };
        Ok(Some(reply))
    }
}
