//! The shapes of an inquiry: an agent asks the person, the person answers later.
//! Declared once here, with the ONE implementation that holds them: an agent whose
//! record declares [`OPEN_FIELD`] (`Option<Vec<Held>>`) and whose bundle lists the
//! verbs `inquiry`, `inquiries` and `answer_inquiry` (the structs below) calls
//! [`ask`], [`list`] and [`answer`]. The open list lives on the agent's own record,
//! written through the kernel's ordinary meta update, so it survives a restart.

use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::refusal::{self, refusal};
use crate::{AgentId, Item, Kernel};

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
    #[serde(default = "default_client_id")]
    #[schemars(schema_with = "client_id")]
    pub reply_client_id: String,
}

/// The caller a message belongs to when it names none: the headless / REPL one.
pub fn default_client_id() -> String {
    "cli".to_string()
}

/// The schema of a client id: text with a character that is not whitespace.
pub fn client_id(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(json!({
        "type": "string",
        "pattern": r"[^\s\x1c-\x1f]"
    }))
    .expect("a client id schema")
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

/// The person's answer, sent to the asker under the inquiry's pid; then the question is closed. Returns {answered, asker}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerInquiry {
    /// The inquiry's pid.
    pub re: i64,
    /// The answer.
    pub answer: Answer,
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

/// The record field holding the open inquiries.
pub const OPEN_FIELD: &str = "open_inquiries";

/// Serialises read-modify-write of every open list (never held across an await).
static HELD: Mutex<()> = Mutex::new(());

/// The open inquiries on the agent's record; a stored list that is not a list of
/// questions is refused, never dropped.
fn open(kernel: &Arc<Kernel>, who: &AgentId) -> Result<Vec<Held>, Value> {
    let stored = kernel
        .agents
        .get(who)
        .map(|e| e.meta.read().expect("meta poisoned").clone())
        .and_then(|m| m.get(OPEN_FIELD).cloned());
    let Some(v) = stored else { return Ok(Vec::new()) };
    serde_json::from_value(v).map_err(|e| {
        let mut r = refusal(refusal::BAD_ARG, format!("{who}: the record's {OPEN_FIELD} is malformed: {e}"));
        refusal::with(&mut r, "arg", json!(OPEN_FIELD));
        r
    })
}

/// Write the list back through the kernel's meta update (which the record writer persists).
fn store(kernel: &Arc<Kernel>, who: &AgentId, list: &[Held]) {
    let mut patch = Map::new();
    patch.insert(OPEN_FIELD.to_string(), json!(list));
    kernel.update(who, patch);
}

/// How many are open — for the agent's reflect (the refusal itself when the stored list is malformed).
#[must_use]
pub fn open_count(kernel: &Arc<Kernel>, who: &AgentId) -> Value {
    open(kernel, who).map_or_else(|e| e, |l| json!(l.len()))
}

/// `inquiry`: held as open; the pid it runs under is its `re`.
pub fn ask(who: &AgentId, a: Inquiry, kernel: &Arc<Kernel>) -> Value {
    let Some(re) = crate::current_task().map(|t| t.0) else {
        return refusal(refusal::NOT_CONNECTED, format!("{who}: inquiry outside a task — nothing to answer under"));
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let request_id = a.request_id.clone();
    let held = Held {
        re,
        asker: crate::current_origin().map(|o| o.0),
        text: a.text.trim().to_string(),
        kind: a.kind,
        options: a.options,
        request_id: a.request_id,
        reply_client_id: a.reply_client_id,
        ts,
    };
    let _g = HELD.lock().expect("inquiries poisoned");
    let mut list = match open(kernel, who) {
        Ok(l) => l,
        Err(e) => return e,
    };
    list.push(held);
    let n = list.len();
    store(kernel, who, &list);
    json!({"accepted": true, "re": re, "request_id": request_id, "open": n})
}

/// `inquiries`: the open ones, oldest first.
pub fn list(who: &AgentId, a: &Inquiries, kernel: &Arc<Kernel>) -> Value {
    let _g = HELD.lock().expect("inquiries poisoned");
    let held = match open(kernel, who) {
        Ok(l) => l,
        Err(e) => return e,
    };
    let mine: Vec<Held> = held
        .into_iter()
        .filter(|q| a.asker.as_deref().is_none_or(|w| q.asker.as_deref() == Some(w)))
        .collect();
    json!({"inquiries": mine})
}

/// `answer_inquiry`: the answer to the asker under the inquiry's pid; closed. The
/// answer goes as `[reply to <re> · <agent id> inquiry]`, the shape every asker settles by.
pub async fn answer(who: &AgentId, a: AnswerInquiry, kernel: &Arc<Kernel>) -> Value {
    let re = a.re;
    let text = a.answer.text();
    let (at, q) = {
        let _g = HELD.lock().expect("inquiries poisoned");
        let mut list = match open(kernel, who) {
            Ok(l) => l,
            Err(e) => return e,
        };
        let Some(at) = list.iter().position(|q| q.re == re) else {
            let mut r = refusal(refusal::NOT_FOUND, format!("{who}: answer_inquiry: no open inquiry {re}"));
            refusal::with(&mut r, "arg", json!("re"));
            return r;
        };
        let q = list.remove(at);
        store(kernel, who, &list);
        (at, q)
    };
    let Some(asker) = q.asker.clone() else {
        return refusal(refusal::NOT_FOUND, format!("{who}: answer_inquiry: the inquiry has no asker to answer"));
    };
    let rid = q.request_id.as_deref().unwrap_or("");
    let body = format!(
        "[reply to {re} · {} inquiry]\n<answer>{text}</answer>\n<request_id>{rid}</request_id>",
        who.as_str()
    );
    let out = Arc::clone(kernel)
        .send(vec![Item::new(
            AgentId::from(asker.as_str()),
            json!({"type": "send", "text": body, "client_id": q.reply_client_id}),
        )])
        .await
        .remove(0);
    match out.result.get("error").and_then(Value::as_str) {
        // the answer did not reach the asker: the question stays open
        Some(e) => {
            let _g = HELD.lock().expect("inquiries poisoned");
            let mut list = match open(kernel, who) {
                Ok(l) => l,
                Err(e) => return e,
            };
            list.insert(at.min(list.len()), q.clone());
            store(kernel, who, &list);
            refusal(refusal::TRANSPORT_ERROR, format!("{who}: answer_inquiry: the answer did not reach {asker}: {e}"))
        }
        None => json!({"answered": re, "asker": asker}),
    }
}
