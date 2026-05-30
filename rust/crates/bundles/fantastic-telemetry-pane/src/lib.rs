//! Live agent-vis GL view for canvas hosts.
//!
//! Answers `get_gl_view`. The host (`canvas_webapp`) compiles the
//! returned JS source via
//! `new Function('THREE','scene','t','onFrame','cleanup', source)`
//! and runs it inside its WebGL scene. The source subscribes to the
//! kernel state stream via `t.subscribeState` and renders each agent
//! as a Three.js Sprite with its display_name, a 10-dot backlog
//! indicator (`+N more` overflow), and a brief border flash on each
//! send/emit. `cleanup.push(...)` registers teardown closures for
//! proper disposal on `remove_agent`.
//!
//! The render path is a pure consumer of the substrate — no
//! `kernel.send` / `emit` / `call` from inside it — so even an
//! instance that visualizes itself does not feedback-loop.
//!
//! ## Verbs
//!
//! - `reflect` → `{id, sentence, verbs}`
//! - `get_gl_view` → `{source, title}` — the JS body the canvas's GL
//!   host consumes, plus the pane's display title.
//! - `boot` / `shutdown` → no-op.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "telemetry_pane.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Embedded GL-view JS body (compiled by the canvas host via
/// `new Function('THREE','scene','t','onFrame','cleanup', source)`).
///
/// Despite the `.glsl` extension on disk this is JavaScript that
/// drives Three.js — kept under a `.glsl` filename so it lives
/// alongside the rest of the bundle's GL-pipeline assets.
pub const TELEMETRY_SOURCE: &str = include_str!("telemetry.glsl");

/// The telemetry-pane bundle.
pub struct TelemetryPaneBundle;

#[async_trait]
impl Bundle for TelemetryPaneBundle {
    fn name(&self) -> &str {
        "telemetry_pane"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "Live agent visualization GL view (canvas peer).",
                "verbs": {
                    "reflect": "Identity + verbs. No args.",
                    "get_gl_view": "Return {source, title} — the JS body the canvas's GL host runs.",
                    "boot": "No-op.",
                    "shutdown": "No-op.",
                }
            }),
            "boot" | "shutdown" => Value::Null,
            "get_gl_view" => json!({
                "source": TELEMETRY_SOURCE,
                "title": "telemetry",
            }),
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests;
