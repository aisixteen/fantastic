//! THE HUMAN'S INQUIRIES (owner 2026-10-11): an agent asks the person, the person
//! answers later. Accepted at once and HELD here as open under the pid the inquiry
//! runs under (`re`), the handle the answer goes back under. Nothing suspends: the
//! asker keeps working, or awaits. The answer is sent to the asker as
//! `[reply to <re> · user inquiry]` — the shape every asker reads (ai_core settles
//! what it is owed by it) — on the asker's own thread, then the question is closed.
//! The verbs are declared in [`crate::verbs`].
//!
//! RAM only: a host restart forgets the open ones (the asker's own history keeps
//! the question).

use std::sync::{Arc, Mutex};

use fantastic_kernel::{AgentId, Item, Kernel};
use serde::Serialize;
use serde_json::{json, Value};

use crate::verbs::{Answer, AnswerInquiry, Inquiries as ListArgs, Inquiry, Kind};

/// One open question, as `inquiries` lists it.
#[derive(Debug, Clone, Serialize)]
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

/// The open questions.
#[derive(Default)]
pub struct Inquiries {
    open: Mutex<Vec<Held>>,
}

fn bad(msg: &str, arg: &str) -> Value {
    json!({"error": format!("user: {msg}"), "reason": "bad_arg", "arg": arg})
}

/// An answer as the asker reads it: words, a list joined, a yes / no.
fn answer_text(v: &Answer) -> String {
    match v {
        Answer::Words(s) => s.trim().to_string(),
        Answer::Picks(a) => a.join(", "),
        Answer::Flag(b) => if *b { "yes" } else { "no" }.to_string(),
    }
}

impl Inquiries {
    /// How many are open.
    #[must_use]
    pub fn count(&self) -> usize {
        self.open.lock().expect("inquiries poisoned").len()
    }

    /// `inquiry`: held as open; the pid it runs under is its `re`.
    pub fn ask(&self, a: Inquiry) -> Value {
        // a question of a kind that offers choices needs them
        if matches!(a.kind, Kind::Single | Kind::Multi) && a.options.is_none() {
            return bad("inquiry: a `single` or `multi` question needs `options`", "options");
        }
        let Some(re) = fantastic_kernel::current_task().map(|t| t.0) else {
            return json!({"error": "user: inquiry outside a task — nothing to answer under", "reason": "not_connected"});
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
        let mut open = self.open.lock().expect("inquiries poisoned");
        open.push(held);
        json!({"accepted": true, "re": re, "request_id": request_id, "open": open.len()})
    }

    /// `inquiries`: the open ones, oldest first.
    pub fn list(&self, a: &ListArgs) -> Value {
        let open = self.open.lock().expect("inquiries poisoned");
        let mine: Vec<&Held> = open
            .iter()
            .filter(|q| a.asker.as_deref().is_none_or(|w| q.asker.as_deref() == Some(w)))
            .collect();
        json!({"inquiries": mine})
    }

    /// `answer_inquiry`: the answer to the asker under the inquiry's pid; closed.
    pub async fn answer(&self, me: &AgentId, a: AnswerInquiry, kernel: &Arc<Kernel>) -> Value {
        let re = a.re;
        let text = answer_text(&a.answer);
        let q = {
            let mut open = self.open.lock().expect("inquiries poisoned");
            let Some(at) = open.iter().position(|q| q.re == re) else {
                return json!({"error": format!("user: answer_inquiry: no open inquiry {re}"), "reason": "not_found", "arg": "re"});
            };
            open.remove(at)
        };
        let Some(asker) = q.asker.clone() else {
            return json!({"error": "user: answer_inquiry: the inquiry has no asker to answer", "reason": "not_found"});
        };
        let rid = q.request_id.as_deref().unwrap_or("");
        let body = format!(
            "[reply to {re} · {} inquiry]\n<answer>{text}</answer>\n<request_id>{rid}</request_id>",
            me.as_str()
        );
        let out = kernel
            .send(vec![Item::new(
                AgentId::from(asker.as_str()),
                json!({"type": "send", "text": body, "client_id": q.reply_client_id}),
            )])
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
