//! THE HUMAN'S INQUIRIES (owner 2026-10-11): an agent asks the person, the person
//! answers later.
//!
//! - `inquiry {text, kind?, options?, request_id?, reply_client_id?}` — ACCEPTED at
//!   once and HELD here as open: `{re, asker, text, kind, options, request_id, ts}`.
//!   `re` is the inquiry's pid (the task it runs under), the handle the answer
//!   goes back under. Nothing suspends: the asker keeps working, or awaits.
//! - `inquiries {asker?}` — the open ones (an asker's only, when named).
//! - `answer_inquiry {re, answer}` — the person's answer, sent to the asker as
//!   `[reply to <re> · user inquiry]` — the shape every asker reads (ai_core
//!   settles what it is owed by it) — on the asker's own thread. Then closed.
//!
//! RAM only: a host restart forgets the open ones (the asker's own history keeps
//! the question).

use std::sync::{Arc, Mutex};

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};

/// The kinds a question takes.
pub const KINDS: [&str; 4] = ["text", "single", "multi", "confirm"];

#[derive(Default)]
pub struct Inquiries {
    open: Mutex<Vec<Value>>,
}

fn bad(msg: &str, arg: &str) -> Value {
    json!({"error": format!("user: {msg}"), "reason": "bad_arg", "arg": arg})
}

/// An answer as the asker reads it: words, a list joined, a yes / no.
fn answer_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Array(a) if !a.is_empty() => Some(a.iter().map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_string)).collect::<Vec<_>>().join(", ")),
        Value::Bool(b) => Some(if *b { "yes" } else { "no" }.to_string()),
        _ => None,
    }
}

impl Inquiries {
    /// `inquiry`: held as open; the pid it runs under is its `re`.
    pub fn ask(&self, p: &Value) -> Value {
        let text = p.get("text").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if text.is_empty() {
            return bad("inquiry: `text` must be a non-empty string", "text");
        }
        let kind = p.get("kind").and_then(Value::as_str).unwrap_or("text");
        if !KINDS.contains(&kind) {
            return bad(&format!("inquiry: `kind` must be one of {}", KINDS.join("|")), "kind");
        }
        let options = p.get("options").cloned().unwrap_or(Value::Null);
        if matches!(kind, "single" | "multi") && !options.as_array().is_some_and(|a| !a.is_empty()) {
            return bad(&format!("inquiry: a `{kind}` question needs `options`"), "options");
        }
        let Some(re) = fantastic_kernel::current_task().map(|t| t.0) else {
            return json!({"error": "user: inquiry outside a task — nothing to answer under", "reason": "not_connected"});
        };
        let asker = fantastic_kernel::current_origin().map(|a| a.0);
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let held = json!({
            "re": re, "asker": asker, "text": text, "kind": kind, "options": options,
            "request_id": p.get("request_id").cloned().unwrap_or(Value::Null),
            "reply_client_id": p.get("reply_client_id").and_then(Value::as_str).unwrap_or("cli"),
            "ts": ts,
        });
        let mut open = self.open.lock().expect("inquiries poisoned");
        open.push(held);
        json!({"accepted": true, "re": re, "request_id": p.get("request_id").cloned().unwrap_or(Value::Null), "open": open.len()})
    }

    /// `inquiries`: the open ones, oldest first.
    pub fn list(&self, p: &Value) -> Value {
        let asker = p.get("asker").and_then(Value::as_str);
        let open = self.open.lock().expect("inquiries poisoned");
        let mine: Vec<Value> = open.iter().filter(|q| asker.is_none_or(|a| q["asker"].as_str() == Some(a))).cloned().collect();
        json!({"inquiries": mine})
    }

    /// `answer_inquiry`: the answer to the asker under the inquiry's pid; closed.
    pub async fn answer(&self, me: &AgentId, p: &Value, kernel: &Arc<Kernel>) -> Value {
        let Some(re) = p.get("re").and_then(Value::as_u64) else {
            return bad("answer_inquiry: `re` (the inquiry's pid) is required", "re");
        };
        let Some(text) = p.get("answer").and_then(answer_text) else {
            return bad("answer_inquiry: `answer` must be words, a list of picks, or true/false", "answer");
        };
        let q = {
            let mut open = self.open.lock().expect("inquiries poisoned");
            let Some(at) = open.iter().position(|q| q["re"].as_u64() == Some(re)) else {
                return json!({"error": format!("user: answer_inquiry: no open inquiry {re}"), "reason": "not_found", "arg": "re"});
            };
            open.remove(at)
        };
        let Some(asker) = q["asker"].as_str() else {
            return json!({"error": "user: answer_inquiry: the inquiry has no asker to answer", "reason": "not_found"});
        };
        let rid = q["request_id"].as_str().unwrap_or("");
        let body = format!("[reply to {re} · {} inquiry]\n<answer>{text}</answer>\n<request_id>{rid}</request_id>", me.as_str());
        let client = q["reply_client_id"].as_str().unwrap_or("cli").to_string();
        let out = kernel
            .send(vec![Item::new(AgentId::from(asker), json!({"type": "send", "text": body, "client_id": client}))])
            .await
            .remove(0);
        match out.result.get("error").and_then(Value::as_str) {
            // the answer did not reach the asker: the question stays open
            Some(e) => {
                self.open.lock().expect("inquiries poisoned").push(q.clone());
                json!({"error": format!("user: answer_inquiry: the answer did not reach {asker}: {e}"), "reason": "refused"})
            }
            None => json!({"answered": re, "asker": asker}),
        }
    }
}
