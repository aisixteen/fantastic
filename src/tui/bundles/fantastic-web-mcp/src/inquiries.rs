//! THE PERSON'S INQUIRIES ON THE MCP LEG (#307): an agent asks the human who drives
//! this kernel over the CLI/MCP leg; the human answers later. Accepted at once and
//! HELD as open under the pid the inquiry runs under (`re`), the handle the answer
//! goes back under: sent to the asker as `[reply to <re> · <leg> inquiry]` (the shape
//! every asker settles by), then closed. The verbs are declared in [`crate::verbs`].
//!
//! DURABLE: the open list lives on the leg's own record (`open_inquiries`), written
//! through the kernel's ordinary meta update — the record writer persists it, the next
//! start reads it back.

use std::sync::{Arc, Mutex};

use fantastic_kernel::{AgentId, Item, Kernel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::verbs::{Answer, AnswerInquiry, Inquiries, Inquiry, Kind};

/// The record field holding the open inquiries.
pub const OPEN_FIELD: &str = "open_inquiries";

/// Serialises read-modify-write of the open list (never held across an await).
static HELD: Mutex<()> = Mutex::new(());

/// One open question, as it sits on the leg's record and as `inquiries` lists it.
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

fn bad(msg: &str, arg: &str) -> Value {
    json!({"error": format!("web_mcp: {msg}"), "reason": "bad_arg", "arg": arg})
}

/// An answer as the asker reads it: words, a list joined, a yes / no.
fn answer_text(v: &Answer) -> String {
    match v {
        Answer::Words(s) => s.trim().to_string(),
        Answer::Picks(a) => a.join(", "),
        Answer::Flag(b) => if *b { "yes" } else { "no" }.to_string(),
    }
}

/// The open inquiries on the leg's record.
fn open(kernel: &Arc<Kernel>, leg: &AgentId) -> Vec<Held> {
    kernel
        .agents
        .get(leg)
        .map(|e| e.meta.read().expect("meta poisoned").clone())
        .and_then(|m| m.get(OPEN_FIELD).cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Write the list back through the kernel's meta update (which the record writer persists).
fn store(kernel: &Arc<Kernel>, leg: &AgentId, list: &[Held]) {
    let mut patch = Map::new();
    patch.insert(OPEN_FIELD.to_string(), json!(list));
    kernel.update(leg, patch);
}

/// How many are open — for the leg's reflect.
#[must_use]
pub fn open_count(kernel: &Arc<Kernel>, leg: &AgentId) -> usize {
    open(kernel, leg).len()
}

/// `inquiry`: held as open; the pid it runs under is its `re`.
pub fn ask(leg: &AgentId, a: Inquiry, kernel: &Arc<Kernel>) -> Value {
    // a question of a kind that offers choices needs them
    if matches!(a.kind, Kind::Single | Kind::Multi) && a.options.is_none() {
        return bad("inquiry: a `single` or `multi` question needs `options`", "options");
    }
    let Some(re) = fantastic_kernel::current_task().map(|t| t.0) else {
        return json!({"error": "web_mcp: inquiry outside a task — nothing to answer under", "reason": "not_connected"});
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let request_id = a.request_id.clone();
    let held = Held {
        re,
        asker: fantastic_kernel::current_origin().map(|o| o.0),
        text: a.text.trim().to_string(),
        kind: a.kind,
        options: a.options,
        request_id: a.request_id,
        reply_client_id: a.reply_client_id,
        ts,
    };
    let _g = HELD.lock().expect("inquiries poisoned");
    let mut list = open(kernel, leg);
    list.push(held);
    let n = list.len();
    store(kernel, leg, &list);
    json!({"accepted": true, "re": re, "request_id": request_id, "open": n})
}

/// `inquiries`: the open ones, oldest first.
pub fn list(leg: &AgentId, a: &Inquiries, kernel: &Arc<Kernel>) -> Value {
    let _g = HELD.lock().expect("inquiries poisoned");
    let mine: Vec<Held> = open(kernel, leg)
        .into_iter()
        .filter(|q| a.asker.as_deref().is_none_or(|w| q.asker.as_deref() == Some(w)))
        .collect();
    json!({"inquiries": mine})
}

/// `answer`: the answer to the asker under the inquiry's pid; closed.
pub async fn answer(leg: &AgentId, a: AnswerInquiry, kernel: &Arc<Kernel>) -> Value {
    let re = a.re;
    let text = answer_text(&a.text);
    let (at, q) = {
        let _g = HELD.lock().expect("inquiries poisoned");
        let mut list = open(kernel, leg);
        let Some(at) = list.iter().position(|q| q.re == re) else {
            return json!({"error": format!("web_mcp: answer: no open inquiry {re}"), "reason": "not_found", "arg": "re"});
        };
        let q = list.remove(at);
        store(kernel, leg, &list);
        (at, q)
    };
    let Some(asker) = q.asker.clone() else {
        return json!({"error": "web_mcp: answer: the inquiry has no asker to answer", "reason": "not_found"});
    };
    let rid = q.request_id.as_deref().unwrap_or("");
    let body = format!(
        "[reply to {re} · {} inquiry]\n<answer>{text}</answer>\n<request_id>{rid}</request_id>",
        leg.as_str()
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
            let mut list = open(kernel, leg);
            list.insert(at.min(list.len()), q.clone());
            store(kernel, leg, &list);
            json!({"error": format!("web_mcp: answer: the answer did not reach {asker}: {e}"), "reason": "refused"})
        }
        None => json!({"answered": re, "asker": asker}),
    }
}
