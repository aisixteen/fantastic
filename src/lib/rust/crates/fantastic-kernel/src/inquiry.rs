//! The shapes of an inquiry: an agent asks the person, the person answers later.
//! Declared once here; the agents that hold inquiries (each in its own way) share
//! these types, so the verbs they declare and the lists they return cannot drift.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

/// Ask the person. ACCEPTED at once ({accepted, re, request_id, open}) and held; the answer arrives LATER as `[reply to <re> · <this agent's id> inquiry]` on your thread. Nothing suspends: keep working, or await.
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

impl Answer {
    /// The answer as the asker reads it: words, a list joined, a yes / no.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Answer::Words(s) => s.trim().to_string(),
            Answer::Picks(a) => a.join(", "),
            Answer::Flag(b) => if *b { "yes" } else { "no" }.to_string(),
        }
    }
}

/// One open question, as an agent holds it and as `inquiries` lists it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Held {
    /// The pid the inquiry runs under: the handle the answer goes back under.
    pub re: i64,
    /// Who asked.
    pub asker: Option<String>,
    /// The question.
    pub text: String,
    /// The shape of the question.
    pub kind: Kind,
    /// The choices.
    pub options: Option<Vec<String>>,
    /// Names the inquiry in the answer.
    pub request_id: Option<String>,
    /// The client the answer is addressed to.
    pub reply_client_id: String,
    /// Epoch seconds the question was held.
    pub ts: f64,
}
