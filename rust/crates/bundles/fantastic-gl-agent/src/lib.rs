//! GL-view-as-a-record bundle.
//!
//! Mirror of `html_agent` for WebGL content. The agent's `gl_source`
//! field IS the GL-view JS body; a canvas host (`canvas_webapp`)
//! probes [`get_gl_view`](#verbs) and compiles the returned source
//! via `new Function('THREE','scene','t','onFrame','cleanup', source)`,
//! running it inside its own per-view `THREE.Group` container — the
//! scene-graph analogue of an `html_agent` iframe.
//!
//! ## Spawn (WS):
//! ```text
//! {"type":"call","target":"core","payload":{
//!   "type":"create_agent",
//!   "handler_module":"gl_agent.tools",
//!   "gl_source":"...JS body...",
//!   "title":"AVS",
//!   "display_name":"AVS bg"
//! },"id":"1"}
//! ```
//!
//! ## Verbs
//!
//! - `reflect` → `{id, sentence, has_source, title, verbs}`
//! - `get_gl_source` → `{source}` (the JS body stored on the
//!   record, or a stub if missing).
//! - `set_gl_source` args `{source:str}` → replaces the source on the
//!   record + emits `gl_source_changed` on self so a canvas hosting
//!   the view reinstalls it in place (dispose the group + recompile)
//!   — same agent id, no canvas refresh.
//! - `get_gl_view` → `{source, title}` — the canvas-host renderable
//!   payload. Byte-identical to Python's.
//! - `boot` / `shutdown` → no-op.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "gl_agent.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Stub source returned by `get_gl_source` when the record has no
/// `gl_source` set yet.
pub const STUB_GL_SOURCE: &str = "// gl_agent — no source set. Use set_gl_source to install one.";

/// Default viewport width served via `get_gl_view`.
pub const DEFAULT_WIDTH: u32 = 800;

/// Default viewport height served via `get_gl_view`.
pub const DEFAULT_HEIGHT: u32 = 600;

/// The GL-view-as-a-record bundle.
pub struct GlAgentBundle;

#[async_trait]
impl Bundle for GlAgentBundle {
    fn name(&self) -> &str {
        "gl_agent"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let agent = match kernel.agents.get(agent_id) {
            Some(e) => Arc::clone(&e),
            None => {
                return Ok(Some(json!({
                    "error": format!("no agent {agent_id}"),
                })))
            }
        };
        let reply = match verb {
            "reflect" => {
                let meta = agent.meta.read().expect("meta poisoned");
                let has_source = meta
                    .get("gl_source")
                    .and_then(Value::as_str)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                let title = meta
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                drop(meta);
                json!({
                    "id": agent_id.as_str(),
                    "sentence": "GL-view-as-record. gl_source stored on agent.json; rendered by canvas hosts that probe get_gl_view.",
                    "has_source": has_source,
                    "title": title,
                    "verbs": {
                        "reflect": "Identity + has_source flag + title. No args.",
                        "get_gl_source": "Return {source: <stored body>} or a stub.",
                        "set_gl_source": "args: source:str (req), title:str?. Patches the record + emits gl_source_changed on self.",
                        "get_gl_view": "Canvas-host renderable payload: {source, title}.",
                        "boot": "No-op.",
                        "shutdown": "No-op.",
                    }
                })
            }
            "boot" | "shutdown" => Value::Null,
            "get_gl_source" => {
                let meta = agent.meta.read().expect("meta poisoned");
                let src = meta
                    .get("gl_source")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| STUB_GL_SOURCE.to_string());
                drop(meta);
                json!({ "source": src })
            }
            "set_gl_source" => {
                let Some(src) = payload.get("source").and_then(Value::as_str) else {
                    return Ok(Some(json!({"error": "gl_agent: source (str) required"})));
                };
                {
                    let mut guard = agent.meta.write().expect("meta poisoned");
                    guard.insert("gl_source".to_string(), Value::String(src.to_string()));
                    if let Some(title) = payload.get("title").and_then(Value::as_str) {
                        guard.insert("title".to_string(), Value::String(title.to_string()));
                    }
                }
                let _ = fantastic_kernel::persistence::persist(&agent);
                // Emit gl_source_changed on self. Carry `id` in the
                // payload: a canvas hosting many GL members needs to
                // know which view changed (unlike an html iframe,
                // which watches only itself).
                kernel
                    .emit(
                        agent_id,
                        json!({"type": "gl_source_changed", "id": agent_id.as_str()}),
                    )
                    .await;
                json!({ "ok": true, "id": agent_id.as_str(), "bytes": src.len() })
            }
            "get_gl_view" => {
                let meta = agent.meta.read().expect("meta poisoned");
                let src = meta
                    .get("gl_source")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let title = meta
                    .get("title")
                    .and_then(Value::as_str)
                    .or_else(|| meta.get("display_name").and_then(Value::as_str))
                    .unwrap_or(agent_id.as_str())
                    .to_string();
                drop(meta);
                // Python parity: {source, title} only — no width/height
                // fields. The canvas.html frontend reads `view.source`
                // and uses the canvas's own sizing.
                json!({
                    "source": src,
                    "title": title,
                })
            }
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests;
