//! THE CANVAS'S HOST NODE: `canvas` under `canvas_page`, holding the canvas's
//! page records (`web_loader`) and its backend (`backend`, one folder per page
//! view). It answers `reflect` and nothing else: it groups, and says so.

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "canvas.tools";

/// The bundle.
#[derive(Default)]
pub struct CanvasNodeBundle;

#[async_trait]
impl Bundle for CanvasNodeBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    async fn handle(&self, _agent_id: &AgentId, payload: &Value, _kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        Ok(match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => None,
            "reflect" => Some(json!({
                "handler_module": HANDLER_MODULE,
                "sentence": "The canvas's host node: its page records (`web_loader`, a kernel_state over .fantastic/web) and its backend (`backend`, one folder per page view, mirroring the page's views).",
                "verbs": {},
            })),
            t => Some(json!({"error": format!("canvas: unknown type {t:?}"), "reason": "unhandled"})),
        })
    }
}
