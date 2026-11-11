//! THE SIDEBAR's agent (`sidebar`): the canvas's left column — the kernels, the agent
//! tree with each chat's state, what waits for the person, and the project's files — as
//! ONE html view its `reflect` returns (compiled in, no record field, no boot code).
//! Its calls are READ verbs on other agents (the `calls` object); it answers `reflect`
//! only. Made under the root by the first-run seed (`blueprint.yaml`).

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "sidebar.tools";

const SENTENCE: &str = "The canvas's left column: kernels, the agent tree with each chat's state, what waits for the person, and the project's files. One html view; reflect only.";

const VIEW: &str = include_str!("sidebar_view.html");

/// What the view calls: the root's reflect and listing, the person's open inquiries, the
/// project's files, each chat's `status` (every other agent: read verbs only), and its own
/// agent's events.
fn calls(id: &str) -> Value {
    json!({
        "kernel": ["reflect", "list_agents"],
        "user": ["inquiries"],
        "files": ["list", "watch_path", "unwatch_path", "open_view"],
        "*": ["status"],
        id: ["watch"],
    })
}

/// The bundle.
pub struct SidebarBundle;

#[async_trait]
impl Bundle for SidebarBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn verb_docs(&self) -> Value {
        json!({"reflect": ""})
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" => Ok(None),
            "reflect" => {
                let id = agent_id.as_str();
                Ok(Some(json!({
                    "id": id,
                    "handler_module": HANDLER_MODULE,
                    "sentence": SENTENCE,
                    "view": {"kind": "html", "bytes": VIEW.replace("{{agent}}", id), "calls": calls(id)},
                })))
            }
            other => Ok(Some(json!({
                "error": format!("sidebar: unknown verb {other:?} — it answers `reflect` only"),
                "reason": "unhandled",
            }))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reflect_carries_the_html_view_and_exactly_its_calls() {
        let kernel = Arc::new(Kernel::new());
        let r = SidebarBundle
            .handle(
                &AgentId::from("sidebar"),
                &json!({"type": "reflect"}),
                &kernel,
            )
            .await
            .unwrap()
            .unwrap();
        let view = &r["view"];
        assert_eq!(view["kind"], "html");
        assert!(
            view["bytes"]
                .as_str()
                .is_some_and(|b| b.contains("fantastic.send")),
            "the body is there"
        );
        assert_eq!(
            view["calls"],
            json!({
                "kernel": ["reflect", "list_agents"],
                "user": ["inquiries"],
                "files": ["list", "watch_path", "unwatch_path", "open_view"],
                "*": ["status"],
                "sidebar": ["watch"],
            })
        );
    }
}
