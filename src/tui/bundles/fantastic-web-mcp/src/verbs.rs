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

/// One call of a batch the carrier hands in.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendItem {
    /// The agent the call goes to.
    #[schemars(length(min = 1))]
    pub target: String,
    /// The call: an object whose `type` is the verb the target answers, with that verb's arguments.
    pub payload: Map<String, Value>,
    /// Seconds; the item is cancelled when it expires.
    pub timeout: Option<f64>,
}

/// Send the batch onward AS THIS LEG — how a carrier's batch becomes the leg's own calls, gated as the leg. Returns {outcomes:[{pid, status, result}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboundSend {
    /// The batch; one item is one dispatch.
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
    /// The choices; required for `single` and `multi`.
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
