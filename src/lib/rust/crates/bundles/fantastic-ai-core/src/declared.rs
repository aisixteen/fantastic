//! THE VERBS OF AN AI CHAT AND OF A PROVIDER, DECLARED ONCE (issue #642): one struct per
//! verb — its fields are the arguments, `Option` for the optional ones, `///` for what
//! each is — then `verbs!` lists them (the chat's in [`chat`]). The generated
//! entry ([`fantastic_kernel::verbs::Parse`]) refuses a call whose arguments are not
//! these, by name, before any handler runs: a handler only ever sees the typed `Call`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

fn default_recall_limit() -> i64 {
    20
}

fn default_grep_limit() -> i64 {
    100
}

fn default_grep_width() -> i64 {
    200
}

/// Start a new incarnation: lazy — it never touches the memory store, it checks the record names one and marks the view for rebuilding (the first turn reads the head from the store).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// An image of a `send`: a file read through the chat's `file_bridge_id`, or a ref kept in its store. Exactly one of the two.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Image {
    /// A path on the chat's file bridge.
    pub path: Option<String>,
    /// A ref of an image already kept in the chat's store.
    #[serde(rename = "ref")]
    pub image_ref: Option<String>,
}

/// Run one turn on `text` and stream its tokens to the caller. The conversation is this agent's own (its memory store); one generation runs at a time and a caller that waits gets a `queued` status. A message that lands while the agent's chain is running is held and answered by the pass that reads it. Returns {response, client_id}.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Send {
    /// The message.
    pub text: String,
    /// Whose turn it is: `cli`, or the tab whose socket filters by it.
    #[serde(default = "fantastic_kernel::inquiry::default_client_id")]
    #[schemars(schema_with = "fantastic_kernel::inquiry::client_id")]
    pub client_id: String,
    /// REPLACES the built system block for this delivery.
    pub system_prompt: Option<String>,
    /// Hand this pass's text to that agent id as a fresh turn, once.
    pub send_to: Option<String>,
    /// Images that ride this message.
    pub images: Option<Vec<Image>>,
    /// Harness text, not a message: stored as a notice, honoured only on a send to self.
    pub notice: Option<bool>,
    /// A reply to a call of a chain that has ended: recorded, never a turn.
    pub late: Option<bool>,
    /// The token of the chain this turn belongs to.
    pub chain: Option<String>,
    /// The sending turn's message id.
    pub msg: Option<i64>,
    /// The message id this one answers.
    pub re: Option<i64>,
    /// Every recipient of the message this one is a copy of.
    pub to: Option<Vec<String>>,
    /// The stream this message was previewed as.
    pub stream_id: Option<String>,
    /// The ids of the agents that sent this call on, for the recursion guard.
    #[serde(rename = "_call_stack")]
    pub call_stack: Option<Vec<String>>,
}

/// Stop this agent's in-flight generation: the provider stream closes, open parts abort, the turn keeps a `[stopped by <you>]` marker. Returns {stopped}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    /// A part of the generation in flight; unknown, nothing stops.
    pub stream_id: Option<String>,
}

/// Pause this agent: stop its in-flight generation and hold every message sent to it, in order, until `resume`. Returns {paused, held}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pause {
    /// Why — kept on the pause row.
    pub reason: Option<String>,
}

/// Resume this agent: write one resumed row, then release the held messages in order, each as a fresh turn after it. Returns {resumed, released}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resume {
    /// Text added to the resumed row.
    pub note: Option<String>,
}

/// A view of this agent's conversation exactly as it HOLDS it in memory — its loaded head, no store read. Returns {rows, next, total, loaded}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rows {
    /// A row id: the page starts after it.
    pub after: Option<i64>,
    /// At most this many rows.
    #[schemars(range(min = 1))]
    pub limit: Option<i64>,
}

/// Ask this agent an inquiry: accepted at once ({accepted, re, request_id}); the answer it later gives with `inquiry_answer` reaches the asker as a message under the inquiry's pid.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inquiry {
    /// The question.
    #[schemars(length(min = 1))]
    pub text: String,
    /// How it is answered: text, single, multi or confirm.
    pub kind: Option<String>,
    /// The choices of a single or multi question.
    pub options: Option<Vec<String>>,
    /// The action a confirm question asks to endorse.
    pub action: Option<Map<String, Value>>,
    /// The asker's own tag, echoed in the reply.
    pub request_id: Option<String>,
    /// Whose conversation the answer returns to.
    #[serde(default = "fantastic_kernel::inquiry::default_client_id")]
    #[schemars(schema_with = "fantastic_kernel::inquiry::client_id")]
    pub reply_client_id: String,
    /// REPLACES the built system block for this delivery.
    pub system_prompt: Option<String>,
}

/// The conversation at DIGEST resolution, oldest first, paged; with `fold`, that fold's title and summary and its members. A row's text is behind `recall` and `grep`. Returns {rows, next, total, fold?}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct History {
    /// A fold or clear id: list ITS members instead.
    pub fold: Option<i64>,
    /// At most this many rows; absent, as many as fit inline.
    #[schemars(range(min = 1))]
    pub limit: Option<i64>,
    /// A row id: page forward from it.
    pub after: Option<i64>,
}

/// Show a HOST agent's panel as a card in this chat, at this place in the conversation; stored as a row, so it survives a reload. Returns {embedded, row, card, view, hint}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Embed {
    /// The host agent, which has a `view` record.
    #[schemars(length(min = 1))]
    pub id: String,
}

/// Take the card of an agent out of this chat: a tombstone row; the agent is never touched. Returns {unembedded}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Unembed {
    /// The agent whose card goes.
    #[schemars(length(min = 1))]
    pub id: String,
}

/// A privacy-filtered snapshot of the queue and the generation in flight: full text only for the asking client's own entries. Answers at once, never queued behind a turn.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Status {
    /// The asking client; absent, the snapshot redacts text everywhere.
    pub client_id: Option<String>,
}

/// The context-budget posture, the last overflow projection and the model's derived reaction to it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextStatus {}

/// Page turns back from the DURABLE store — the full conversation, anything compaction dropped is one call away. Read-only. Returns {messages, total, truncated, note?}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Recall {
    /// A row id: that row whole (query, limit and before are then ignored).
    pub id: Option<i64>,
    /// A substring of a serialized turn, tool args and replies included.
    #[schemars(length(min = 1))]
    pub query: Option<String>,
    /// At most this many turns.
    #[schemars(range(min = 1, max = 100))]
    #[serde(default = "default_recall_limit")]
    pub limit: i64,
    /// A store index: page backward from it.
    pub before: Option<i64>,
}

/// Find the LINE that mentions something ANYWHERE in memory — every row ever written or received, behind every clear, inside every fold, and the reply bodies still held. Returns {matches:[{id, line, text}], total, truncated}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Grep {
    /// A case-insensitive substring.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub query: String,
    /// Row, pid or fold ids to search; a fold id searches its members.
    pub ids: Option<Vec<i64>>,
    /// At most this many matches.
    #[schemars(range(min = 1, max = 100))]
    #[serde(default = "default_grep_limit")]
    pub limit: i64,
    /// Characters of each matched line.
    #[schemars(range(min = 20, max = 20000))]
    #[serde(default = "default_grep_width")]
    pub width: i64,
}

/// Rebuild this agent's VIEW from its store: fold a head longer than the budget, fit it, ask the model which rows it wants in full, persist the id list. Returns {kept, expanded, dropped, folded, budget, why, degraded}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextRecall {
    /// Mechanical fit only — no model call.
    pub dry: Option<bool>,
}

/// Start a NEW THREAD on the same store: one clear-fold row whose members are every row since the previous clear; nothing is deleted. Returns {cleared, rows}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextClear {
    /// A sentence naming the thread (whitespace-collapsed, 200 characters).
    pub why: Option<String>,
}

/// Fold a range of consecutive rows of your view into ONE fold row; its members stay reachable by `history`, `recall` and `grep`. Returns {folded, rows, summarized}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextFold {
    /// The first row of the range; absent, the first row after the last fold or clear.
    pub from: Option<i64>,
    /// The last row of the range; absent, the last row of the view.
    pub to: Option<i64>,
    /// The fold's text, used verbatim; absent, the fold summarizer writes it.
    #[schemars(length(min = 1))]
    pub summary: Option<String>,
}

/// Compact this agent's OWN store: drop the rows the model can no longer see and keep every fold. Waits for a running turn. Returns {pruned, kept_folds, unanchored}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextForget {
    /// Also drop every row before the last clear.
    pub include_cleared: Option<bool>,
}

/// The agent and the row of a fork of ANOTHER agent.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForkFrom {
    /// The agent to fork.
    #[schemars(length(min = 1))]
    pub agent: String,
    /// The row to fork it at.
    pub at: i64,
}

/// Start a NEW agent from this one's first rows: the same card and a copy of every row up to `at`, in one all-or-nothing append. Returns {id, copied, at}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fork {
    /// The row id to fork at (required unless `from` names another agent).
    pub at: Option<i64>,
    /// The new agent's name.
    pub name: Option<String>,
    /// The new agent's id.
    pub id: Option<String>,
    /// Fork THAT agent instead: sent to it as a gated call.
    pub from: Option<ForkFrom>,
}

/// Mark THIS agent's history at its newest row, a turn boundary outside any fold, so `fork {at}` there is always valid. The last 20 marks are kept. Returns {marked, at, label}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    /// The mark's name.
    #[schemars(length(min = 1))]
    pub label: String,
    /// A note kept with it.
    pub note: Option<String>,
    /// A free-form tag, such as a repo commit.
    pub version: Option<String>,
}

/// Which marks `fork_points` lists.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForkFilter {
    /// A substring of the label.
    pub label: Option<String>,
    /// The exact version.
    pub version: Option<String>,
}

/// THIS agent's marks, oldest first. Returns {points:[{agent, at, label, note, version, rows_after, model, warm}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForkPoints {
    /// Which marks.
    pub filter: Option<ForkFilter>,
}

/// Announce every row this agent appends to YOUR inbox as {type:'row', source, row:{id, kind, role, bytes}} — content-free; with `body` the row's `content` too. Needs a registered inbox. Returns {watching, watcher, body}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Watch {
    /// Carry the rows' content.
    pub body: Option<bool>,
    /// An inbox id to deliver to (operator only).
    pub watcher: Option<String>,
}

/// Stop announcing rows to you. Returns {watching:false}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Unwatch {
    /// The inbox to stop (operator only).
    pub watcher: Option<String>,
}

/// An image of a `complete` request, as kept in the chat's store.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    /// The ref the chat kept it under.
    #[serde(rename = "ref")]
    pub image_ref: String,
    /// Its media type.
    pub media_type: String,
}

/// One stateless completion pass: no memory, no calls, no conversation kept. Each content token is emitted on this agent's stream as {type:'provider_token', rid, text}. Returns {rid, model, prompt_tokens, completion_tokens, stop_reason} or {rid, error, transport}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Complete {
    /// Role and content, as ai_core assembles them (required, except on the call that carries an image).
    pub messages: Option<Vec<Map<String, Value>>>,
    /// An image of the request, its bytes the body of this call (a provider that takes images).
    pub image: Option<ImageRef>,
    /// The model of this pass; absent, this provider's default model.
    pub model: Option<String>,
    /// This call's output bound; absent, the record's.
    pub max_tokens: Option<i64>,
    /// Tags every token event.
    #[schemars(length(min = 1))]
    pub rid: String,
}

/// The models this provider serves and its default. Returns {models, default}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Models {}

/// Set this provider's default model (one of `models` when the list is known); a chat that names no model completes on it. Returns {default}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetDefaultModel {
    /// The model name.
    #[schemars(length(min = 1))]
    pub model: String,
}

/// Every verb of an AI chat: the entry the chat bundle's agents answer.
pub mod chat {
    use super::*;

    fantastic_kernel::verbs! {
        "boot" => Boot,
        "send" => Send,
        "stop" => Stop,
        "pause" => Pause,
        "resume" => Resume,
        "rows" => Rows,
        "inquiry" => Inquiry,
        "history" => History,
        "embed" => Embed,
        "unembed" => Unembed,
        "status" => Status,
        "context_status" => ContextStatus,
        "recall" => Recall,
        "grep" => Grep,
        "context_recall" => ContextRecall,
        "context_clear" => ContextClear,
        "context_fold" => ContextFold,
        "context_forget" => ContextForget,
        "fork" => Fork,
        "mark" => Mark,
        "fork_points" => ForkPoints,
        "watch" => Watch,
        "unwatch" => Unwatch,
    }
}

/// Every verb of a provider backend: what its agents answer (a chat's verbs belong to
/// an `ai_chat.tools` agent that names the provider; it completes here).
pub mod provider {
    use super::*;

    fantastic_kernel::verbs! {
        "boot" => Boot,
        "complete" => Complete,
        "models" => Models,
        "set_default_model" => SetDefaultModel,
    }
}
