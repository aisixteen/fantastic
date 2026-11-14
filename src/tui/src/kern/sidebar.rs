//! THE SIDEBAR's agent (`sidebar`): the canvas's left column — the kernels, the agent
//! tree with each chat's state, what waits for the person, and the project's files — as
//! ONE html view its `reflect` returns (compiled in, no record field, no boot code).
//! Its calls are READ verbs on other agents (the `calls` object); it answers `reflect`
//! only. Made under the root by the first-run seed (`blueprint.yaml`).

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "sidebar.tools";

pub mod record;
pub mod verbs;

/// What the bundle is for: its one short man text; how to call it is rendered from its
/// declared verbs (`docs`, `reflect`).
const MAN: &str = "The canvas's left column as one html view its `reflect` returns: the kernels, the agent tree with each chat's state, what waits for the person, and the project's files.";

const VIEW: &str = include_str!("sidebar_view.html");

/// What the view calls: the root's reflect and listing, the person's open inquiries, the
/// project's files (listed, held with `watch_path`, heard through `watch`), each chat's `status` (every other agent: read verbs only), and its own
/// agent's events.
fn calls(id: &str) -> Value {
    json!({
        "kernel": ["reflect", "list_agents"],
        "user": ["inquiries"],
        "files": ["list", "watch_path", "unwatch_path", "open_view", "watch"],
        "*": ["status"],
        id: ["watch"],
    })
}

/// The bundle.
pub struct SidebarBundle;

#[async_trait]
impl Handler for SidebarBundle {
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
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match call {
            verbs::Call::Reflect(_) => {
                let id = agent_id.as_str();
                Ok(Some(json!({
                    "id": id,
                    "handler_module": HANDLER_MODULE,
                    "sentence": MAN,
                    "view": {"kind": "html", "bytes": VIEW.replace("{{agent}}", id), "calls": calls(id)},
                })))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_sidebar_answers_reflect_and_refuses_the_rest_by_declaration() {
        let kernel = Arc::new(Kernel::new());
        kernel.register(fantastic_kernel::Agent::new(
            AgentId::from("sidebar"),
            None,
            None,
            Default::default(),
            std::path::PathBuf::new(),
            false,
        ));
        let ask = |payload: Value| {
            let kernel = Arc::clone(&kernel);
            async move {
                fantastic_kernel::Bundle::handle_binary(&SidebarBundle, &AgentId::from("sidebar"), payload, None, &kernel)
                    .await
                    .unwrap()
                    .0
            }
        };
        let r = ask(json!({"type": "launch"})).await.unwrap();
        assert_eq!(r["reason"], "unhandled", "{r}");
        assert!(r["error"].as_str().unwrap().contains("reflect"), "{r}");
        for v in ["reflect"] {
            let r = ask(json!({"type": v, "zzz_unknown": 1})).await.unwrap();
            assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("zzz_unknown")), "{v}: {r}");
        }
        let r = ask(json!({"type": "reflect"})).await.unwrap();
        assert_eq!(r["view"]["kind"], "html", "{r}");
    }

    #[tokio::test]
    async fn reflect_carries_the_html_view_and_exactly_its_calls() {
        let kernel = Arc::new(Kernel::new());
        let r = SidebarBundle
            .handle(
                &AgentId::from("sidebar"),
                verbs::Call::Reflect(fantastic_kernel::declared::common::Reflect {
                    detail: None,
                    tree: None,
                    show_subtrees: None,
                    ignore_subtrees: None,
                    handler_modules: None,
                    bundles: None,
                    bundle_names: None,
                    limit: None,
                    after: None,
                }),
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
                "files": ["list", "watch_path", "unwatch_path", "open_view", "watch"],
                "*": ["status"],
                "sidebar": ["watch"],
            })
        );
    }
}
