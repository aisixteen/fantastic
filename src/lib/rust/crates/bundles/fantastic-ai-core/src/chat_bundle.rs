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
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Kernel};
use schemars::JsonSchema;
use serde_json::{json, Value};

use crate::agent_loop::BackendConfig;
use crate::declared::chat;
use crate::record::AiRecord;
use crate::remote_provider::{chat_provider, PROVIDER_FIELD};
use crate::verbs;

/// The handler module this bundle registers under.
pub const HANDLER_MODULE: &str = "ai_chat.tools";
const NAME: &str = "ai_chat";
/// What the bundle is for — its one short man text.
const MAN: &str = "An AI agent: it holds a conversation in its memory store, folds and recalls it to fit the model's window, reads the calls the model writes and routes each to the agent it names, and completes every turn through the provider agent its record names.";

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

/// The fields a chat's record carries, beside the ones every record has.
#[derive(Debug, JsonSchema)]
pub struct ChatRecord {
    /// The provider agent that completes this chat's turns (`<guid>|<id>` on a connected kernel).
    pub provider_id: Option<String>,
    /// The model this chat asks its provider for.
    pub model: Option<String>,
    /// How the conversation is kept in the model's view; `provider_cache` asks the provider for its cache markers.
    pub caching: Option<crate::record::CachingMode>,
    /// `true` teaches the titles how-to: a stripped body keeps its title.
    #[schemars(default = "default_off")]
    pub context_titles: Option<bool>,
    /// `true` teaches the full-async how-to: replies arrive as rows.
    #[schemars(default = "default_off")]
    pub context_async: Option<bool>,
    /// The fields every ai_core agent takes.
    #[serde(flatten)]
    pub ai: AiRecord,
}

fn default_off() -> Option<bool> {
    Some(false)
}

fantastic_kernel::record!(ChatRecord);

/// The `ai_chat.tools` bundle.
pub struct AiChatBundle;

#[async_trait]
impl Handler for AiChatBundle {
    type Call = chat::Call;

    fn name(&self) -> &str {
        NAME
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        chat::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        chat::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let provider = |_client: &str| chat_provider(agent_id, kernel);
        let reflect = || {
            let mut out = verbs::reflect_base(agent_id, kernel, MAN, CFG.model_window);
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
            Value::Object(out)
        };
        Ok(Some(
            verbs::handle(agent_id, call, kernel, CFG, provider, reflect).await?,
        ))
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
