//! `ai_chat.tools` — THE AI AGENT, apart from any provider.
//!
//! It holds the conversation (memory, folds, history, the `<call>` parse and
//! route, fork, watch, embed: every ai_core verb) and completes each turn through
//! the provider agent its record names (`provider_id`), by `complete`
//! ([`crate::remote_provider`]). It knows no provider's kind, model, endpoint or
//! key: those live on the provider agent. A provider on a connected kernel
//! (`<guid>|<id>`) serves it the same way. Roles and other meta bundles extend
//! THIS bundle, never a provider.

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

use crate::agent_loop::BackendConfig;
use crate::remote_provider::{chat_provider, PROVIDER_FIELD};
use crate::verbs;

/// The handler module this bundle registers under.
pub const HANDLER_MODULE: &str = "ai_chat.tools";
const NAME: &str = "ai_chat";
/// What `reflect` says the agent is.
pub const SENTENCE: &str =
    "An AI chat: holds its conversation and completes each turn through its provider agent (`provider_id`).";

const CFG: BackendConfig = BackendConfig {
    name: NAME,
    model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
    // the provider decides: a provider that cannot place cache markers ignores them; where marks go is this chat's `caching`
    provider_cache: true,
};

/// The chat's own view (html, plain JS): the canvas draws the chat from it. `{{agent}}` is the
/// chat's id, filled at reflect time.
const CHAT_VIEW: &str = include_str!("chat_view.html");

/// The verbs the view calls on its own chat agent.
pub const VIEW_CALLS: [&str; 9] = [
    "watch",
    "reflect",
    "history",
    "rows",
    "status",
    "send",
    "stop",
    "context_clear",
    "unembed",
];
/// What the view reads and answers on `user`: the inquiries this chat asked the person.
pub const VIEW_USER_CALLS: [&str; 2] = ["inquiries", "answer_inquiry"];

const README: &str = "# ai_chat\n\nThe AI agent apart from any provider: it keeps the conversation \
and completes each turn through the provider agent named by `provider_id` (`complete`). Grant it \
`can_call {<provider>: [complete, watch]}`; the provider admits it in `callable_by`. Model, endpoint \
and key are the provider's; the cache marks are this chat's (`caching`): the provider forwards them.\n";

/// The `ai_chat.tools` bundle.
pub struct AiChatBundle;

#[async_trait]
impl Bundle for AiChatBundle {
    fn name(&self) -> &str {
        NAME
    }

    fn verb_docs(&self) -> Value {
        let mut docs = verbs::verb_docs();
        if let Some(m) = docs.as_object_mut() {
            for v in ["complete", "models", "set_default_model"] {
                m.remove(v); // a provider's verbs, not a chat's
            }
        }
        docs
    }

    fn emits_doc(&self) -> Option<Value> {
        Some(verbs::emits())
    }

    fn cancel_doc(&self) -> Option<Value> {
        Some(Value::String(verbs::CANCEL_DOC.to_string()))
    }

    fn notes_doc(&self) -> Option<Value> {
        Some(verbs::notes())
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
        if verb == "reflect" {
            let mut out = verbs::reflect_base(agent_id, kernel, SENTENCE, CFG.model_window);
            let meta = crate::helpers::agent_meta(agent_id, kernel);
            out.insert(
                PROVIDER_FIELD.into(),
                meta.get(PROVIDER_FIELD).cloned().unwrap_or(Value::Null),
            );
            // HOW IT IS DRAWN: an html view, compiled in (no record field holds it)
            let id = agent_id.as_str();
            out.insert(
                "view".into(),
                json!({
                    "kind": "html",
                    "bytes": CHAT_VIEW.replace("{{agent}}", id),
                    "calls": {id: VIEW_CALLS, "user": VIEW_USER_CALLS},
                }),
            );
            return Ok(Some(Value::Object(out)));
        }
        if matches!(verb, "complete" | "models" | "set_default_model") {
            return Ok(Some(fantastic_kernel::refusal::refusal(
                fantastic_kernel::refusal::UNHANDLED,
                format!("{NAME}: `{verb}` is a provider's verb; this chat completes through its {PROVIDER_FIELD} (ask it there)"),
            )));
        }
        let provider = |_client: &str| chat_provider(agent_id, kernel);
        if let Some(reply) = verbs::handle_common(agent_id, payload, kernel, CFG, provider).await? {
            return Ok(Some(reply));
        }
        Ok(Some(fantastic_kernel::refusal::refusal(
            fantastic_kernel::refusal::UNHANDLED,
            format!("{NAME}: unknown type {}", json!(verb)),
        )))
    }

    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        verbs::on_delete(agent_id, kernel).await;
        Ok(())
    }

    /// A cancel of this chat's turn closes it (the provider's pass, a call of the turn,
    /// is cancelled with it).
    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &fantastic_kernel::TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await
    }
}
