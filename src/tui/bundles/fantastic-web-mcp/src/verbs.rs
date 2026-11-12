//! The verbs the MCP leg declares (issue #642): one struct per verb, then `verbs!` —
//! the list the agent's own entry checks every call against, and that `reflect` and
//! `docs` render. `reflect` is declared once, in the kernel.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Make the project's MCP config right (when the record names `file_bridge_id`): `mcpServers.fantastic` is this process's binary with `mcp`, written only when it changed. Answers nothing, or {error, reason: "mcp_config"} when the config could not be made right.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// The call: the verb the target answers in `type`, and that verb's arguments beside it.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Payload {
    /// The verb.
    #[schemars(length(min = 1))]
    pub r#type: String,
    /// The verb's arguments — they come from reflect on the target.
    #[serde(flatten)]
    pub args: Map<String, Value>,
}

impl Payload {
    /// The call as the kernel dispatches it: `type` first, then the arguments.
    #[must_use]
    pub fn into_value(self) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), Value::String(self.r#type));
        m.extend(self.args);
        Value::Object(m)
    }
}

/// One call of a batch the carrier hands in.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendItem {
    /// The agent the call goes to, or `kernel` for the root.
    #[schemars(length(min = 1))]
    pub target: String,
    /// The call: `type` is the verb, beside its arguments.
    pub payload: Payload,
    /// Seconds; the item is cancelled when it expires.
    pub timeout: Option<f64>,
}

/// Ask the kernel what exists. With NO arguments: the kernel's own primer — `tree` (every agent id) and `bundles` (what it can run). With `target`: that agent describes itself — its sentence, its verbs and their args. Extra arguments are passed through to the reflect, e.g. tree="ids". Reflect before you send: verbs are discovered here, never guessed.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReflectTool {
    /// Agent id to reflect. Omit for the kernel's own primer.
    #[schemars(length(min = 1))]
    pub target: Option<String>,
    /// Arguments of the reflect itself.
    #[serde(flatten)]
    pub args: Map<String, Value>,
}

/// Dispatch to agents — the kernel's one verb, batch form: send([{target, payload:{type:<verb>, ...args}, timeout?}]) -> [{pid, status: completed|failed|cancelled|timed_out, result}]. Discover verbs with reflect on the target; `kernel` is the alias for the root. A `timeout` (seconds) on an item CANCELS it when it expires. The call answers when it ends; each call is a kernel task, listed by send [{target:"kernel", payload:{type:"ps"}}]. A refusal (no grant) comes back as a result with an `error`, and a ledger row, like any other call.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendTool {
    /// The batch. One item is one dispatch.
    #[schemars(length(min = 1))]
    pub items: Vec<SendItem>,
}

/// The `reflect` tool, declared like a verb: its schema is the tool's input schema.
pub static REFLECT_TOOL: fantastic_kernel::Verb = fantastic_kernel::Verb::of::<ReflectTool>("reflect");
/// The `send` tool.
pub static SEND_TOOL: fantastic_kernel::Verb = fantastic_kernel::Verb::of::<SendTool>("send");

/// Send the batch onward AS THIS LEG — how a carrier's batch becomes the leg's own calls, gated as the leg. Returns {outcomes:[{pid, status, result}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboundSend {
    /// The batch; one item is one dispatch.
    #[schemars(length(min = 1))]
    pub items: Vec<SendItem>,
}

/// The shapes a question takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Free text (the default).
    #[default]
    Text,
    /// One option.
    Single,
    /// Several options.
    Multi,
    /// Yes or no.
    Confirm,
}

/// Ask the person who drives this leg. ACCEPTED at once ({accepted, re, request_id, open}) and held; the answer arrives LATER as `[reply to <re> · <this leg> inquiry]` on the asker's thread. Nothing suspends: keep working, or await.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inquiry {
    /// The question.
    #[schemars(length(min = 1), regex(pattern = "\\S"))]
    pub text: String,
    /// The shape of the question.
    #[serde(default)]
    pub kind: Kind,
    /// The choices offered for `single` and `multi`.
    #[schemars(length(min = 1))]
    pub options: Option<Vec<String>>,
    /// Names this inquiry in the answer.
    pub request_id: Option<String>,
    /// The client the answer is addressed to.
    #[serde(default = "cli")]
    #[schemars(length(min = 1))]
    pub reply_client_id: String,
}

fn cli() -> String {
    "cli".to_string()
}

/// The open questions, oldest first. Returns {inquiries:[{re, asker, text, kind, options, request_id, reply_client_id, ts}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inquiries {
    /// Only this asker's questions.
    #[schemars(length(min = 1))]
    pub asker: Option<String>,
}

/// The person's answer: words, a list of picks, or yes / no.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Answer {
    /// Words.
    Words(#[schemars(length(min = 1), regex(pattern = "\\S"))] String),
    /// A list of picks.
    Picks(#[schemars(length(min = 1))] Vec<String>),
    /// Yes (`true`) or no.
    Flag(bool),
}

/// The person's answer, sent to the asker under the inquiry's pid; then the question is closed. Returns {answered, asker}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerInquiry {
    /// The inquiry's pid.
    pub re: i64,
    /// The answer.
    pub text: Answer,
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "inbound_send" => InboundSend,
    "inquiry" => Inquiry,
    "inquiries" => Inquiries,
    "answer" => AnswerInquiry,
}
