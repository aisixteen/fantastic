//! THE BROWSER VIEW'S LEG, as an ordinary bundle built outside the kernel tree.
//!
//! A `web_ui` agent is a child of a `web` host. It declares ONE websocket route
//! through the duck-typed `get_routes` verb; `web` mounts it, and every frame on
//! that socket runs the web_ws frame protocol (`call`, `emit`, `watch`,
//! `state_subscribe`) AS this agent — so the browser View is gated, ledgered and
//! granted like any other agent, never as the operator.
//!
//! Its own verbs: `reflect`, `boot` (no-op: the host owns the route) and
//! `get_routes`.

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// The `handler_module` this bundle registers under.
pub const HANDLER_MODULE: &str = "web_ui.tools";

/// The socket the page connects to.
pub const WS_PATH: &str = "/web_ui/ws";

/// The bundle. Stateless: everything lives on the socket and the record.
#[derive(Default)]
pub struct WebUiBundle;

#[async_trait]
impl Bundle for WebUiBundle {
    fn name(&self) -> &str {
        "web_ui"
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(Some(match verb {
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "The browser View's leg: one websocket, every frame runs as this agent.",
                "ws_path": WS_PATH,
                "verbs": {
                    "reflect": "No args.",
                    "boot": "No-op; the parent `web` host owns the route.",
                    "get_routes": "Declares {routes:[{kind:'websocket', path}]} for the host to mount.",
                },
            }),
            "boot" => json!({"booted": true}),
            "get_routes" => json!({"routes": [{"kind": "websocket", "path": WS_PATH}]}),
            other => json!({"error": format!("web_ui: unknown type {other:?}"), "reason": "unhandled"}),
        }))
    }
}
