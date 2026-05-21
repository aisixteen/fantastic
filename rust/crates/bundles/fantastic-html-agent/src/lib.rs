//! UI-as-a-record bundle.
//!
//! The agent's body lives at `<agent_dir>/index.html` — a plain file
//! next to `agent.json`. The record stays lean (identity + display
//! fields); the HTML is editable in any text editor / shell without
//! JSON-escape gymnastics. The `web` bundle serves it at `/<id>/`
//! with `transport.js` auto-injected.
//!
//! ## Spawn (WS):
//! ```text
//! {"type":"call","target":"core","payload":{
//!   "type":"create_agent",
//!   "handler_module":"html_agent.tools",
//!   "html":"<h1>hi</h1>",
//!   "display_name":"Panel"
//! },"id":"1"}
//! ```
//!
//! ## Verbs
//!
//! - `reflect` → `{id, sentence, has_html, display_name}`
//! - `render_html` → `{html}` (stored body, or a stub if missing)
//! - `set_html` → writes the new body + emits `reload_html` on self
//!   so connected pages refresh.
//! - `boot` → migrates `html` field from the record into
//!   `index.html` on first run (idempotent), then no-op.
//! - `shutdown` → no-op.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "html_agent.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

const STUB_HTML: &str =
    "<html><body><p>html_agent — no body set.</p><p>Use set_html.</p></body></html>";

/// The HTML-as-a-record bundle.
pub struct HtmlAgentBundle;

#[async_trait]
impl Bundle for HtmlAgentBundle {
    fn name(&self) -> &str {
        "html_agent"
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
                let has = html_path(&agent.root_path).exists();
                let display_name = agent.display_name().unwrap_or_else(|| agent.id.0.clone());
                json!({
                    "id": agent_id.as_str(),
                    "sentence": "UI-as-a-record. Body at <agent_dir>/index.html.",
                    "has_html": has,
                    "display_name": display_name,
                    "verbs": {
                        "reflect": "Identity + has_html flag. No args.",
                        "render_html": "Return {html: <stored body>}.",
                        "set_html": "args: html:str (req). Writes index.html + emits reload_html.",
                        "get_webapp": "Canvas-renderable descriptor: {url, default_width, default_height, title}.",
                        "boot": "No-op.",
                        "shutdown": "No-op.",
                    }
                })
            }
            "boot" => {
                // Idempotent: if the agent was created with a `html`
                // field on the create payload, the substrate's meta-
                // flatten persisted it onto the record. Migrate that
                // into index.html on first boot so refresh works for
                // the initial visit; strip the field from meta so the
                // on-disk record stays lean.
                let mut migrated = false;
                let meta = agent.meta.read().expect("meta poisoned").clone();
                if let Some(html) = meta.get("html").and_then(Value::as_str) {
                    let _ = write_html(&agent.root_path, html);
                    migrated = true;
                }
                if migrated {
                    let mut patch = serde_json::Map::new();
                    patch.insert("html".to_string(), Value::Null);
                    // update_meta merges keys (Null deletes); the
                    // persistence write happens below.
                    let mut guard = agent.meta.write().expect("meta poisoned");
                    guard.remove("html");
                    drop(guard);
                    let _ = fantastic_kernel::persistence::persist(&agent);
                }
                Value::Null
            }
            "shutdown" => Value::Null,
            "render_html" => {
                let body = read_html(&agent.root_path).unwrap_or_else(|| STUB_HTML.to_string());
                json!({ "html": body })
            }
            "set_html" => {
                let Some(html) = payload.get("html").and_then(Value::as_str) else {
                    return Ok(Some(json!({"error": "set_html requires html"})));
                };
                if let Err(e) = write_html(&agent.root_path, html) {
                    return Ok(Some(json!({"error": format!("write html: {e}")})));
                }
                // Emit reload_html on self so transport.js subscribers
                // refresh.
                kernel.emit(agent_id, json!({"type": "reload_html"})).await;
                json!({"id": agent_id.as_str(), "html_bytes": html.len()})
            }
            // Canvas-renderable descriptor (Python parity). Without
            // this, canvas_backend.add_agent refuses html_agent because
            // it can't iframe a member that doesn't answer get_webapp.
            "get_webapp" => {
                let meta = agent.meta.read().expect("meta poisoned");
                let default_width = meta.get("width").and_then(Value::as_u64).unwrap_or(480);
                let default_height = meta.get("height").and_then(Value::as_u64).unwrap_or(360);
                let display_name = agent.display_name().unwrap_or_else(|| "html".to_string());
                drop(meta);
                json!({
                    "url": format!("/{}/", agent_id.as_str()),
                    "default_width": default_width,
                    "default_height": default_height,
                    "title": display_name,
                })
            }
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

fn html_path(root: &Path) -> std::path::PathBuf {
    root.join("index.html")
}

fn read_html(root: &Path) -> Option<String> {
    fs::read_to_string(html_path(root)).ok()
}

fn write_html(root: &Path, body: &str) -> std::io::Result<()> {
    fs::create_dir_all(root)?;
    fs::write(html_path(root), body)
}

#[cfg(test)]
mod tests;
