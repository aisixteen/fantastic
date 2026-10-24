//! THE PERSON'S INQUIRIES ON THE MCP LEG (#307): an agent asks the human who drives
//! this kernel over the CLI/MCP leg; the human answers later.
//!
//! - `inquiry {text, kind?, options?, request_id?, reply_client_id?}` — ACCEPTED at
//!   once and HELD as open: `{re, asker, text, kind, options, request_id, ts}`. `re`
//!   is the pid the inquiry runs under, the handle the answer goes back under.
//! - `inquiries {asker?}` — the open ones, oldest first.
//! - `answer {re, text}` — the person's answer, sent to the asker as
//!   `[reply to <re> · <leg> inquiry]` (the shape every asker settles by), then closed.
//!
//! DURABLE: the open list lives on the leg's own record (`open_inquiries`), written
//! through the kernel's ordinary meta update — the record writer persists it, the next
//! start reads it back.

use std::sync::{Arc, Mutex};

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};

/// The record field holding the open inquiries.
pub const OPEN_FIELD: &str = "open_inquiries";

/// The kinds a question takes.
pub const KINDS: [&str; 4] = ["text", "single", "multi", "confirm"];

/// Serialises read-modify-write of the open list (never held across an await).
static HELD: Mutex<()> = Mutex::new(());

pub const INQUIRY_DOC: &str = "args: text:str (req), kind:str? (text|single|multi|confirm, default text), options:list? (required for single/multi), request_id:str?, reply_client_id:str? — ACCEPTED at once ({accepted, re, request_id, open}) and held for the person driving this leg; the answer arrives LATER as `[reply to <re> · <this leg> inquiry]` on the asker's thread. Nothing suspends: keep working, or await.";
pub const INQUIRIES_DOC: &str = "args: asker:str? — {inquiries: [{re, asker, text, kind, options, request_id, reply_client_id, ts}]}: the open questions, oldest first (one asker's, when named).";
pub const ANSWER_DOC: &str = "args: re:int (req — the inquiry's pid), text: str | list | bool (req) — the person's answer, sent to the asker under the inquiry's pid; then closed.";

fn bad(msg: &str, arg: &str) -> Value {
    json!({"error": format!("web_mcp: {msg}"), "reason": "bad_arg", "arg": arg})
}

/// An answer as the asker reads it: words, a list joined, a yes / no.
fn answer_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Array(a) if !a.is_empty() => Some(
            a.iter()
                .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_string))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Value::Bool(b) => Some(if *b { "yes" } else { "no" }.to_string()),
        _ => None,
    }
}

/// The open inquiries on the leg's record.
fn open(kernel: &Arc<Kernel>, leg: &AgentId) -> Vec<Value> {
    kernel
        .agents
        .get(leg)
        .map(|e| e.meta.read().expect("meta poisoned").clone())
        .and_then(|m| m.get(OPEN_FIELD).and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// Write the list back through the kernel's meta update (which the record writer persists).
fn store(kernel: &Arc<Kernel>, leg: &AgentId, list: Vec<Value>) {
    let mut patch = Map::new();
    patch.insert(OPEN_FIELD.to_string(), Value::Array(list));
    kernel.update(leg, patch);
}

/// How many are open — for the leg's reflect.
#[must_use]
pub fn open_count(kernel: &Arc<Kernel>, leg: &AgentId) -> usize {
    open(kernel, leg).len()
}

/// `inquiry`: held as open; the pid it runs under is its `re`.
pub fn ask(leg: &AgentId, p: &Value, kernel: &Arc<Kernel>) -> Value {
    let text = p
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if text.is_empty() {
        return bad("inquiry: `text` must be a non-empty string", "text");
    }
    let kind = p.get("kind").and_then(Value::as_str).unwrap_or("text");
    if !KINDS.contains(&kind) {
        return bad(
            &format!("inquiry: `kind` must be one of {}", KINDS.join("|")),
            "kind",
        );
    }
    let options = p.get("options").cloned().unwrap_or(Value::Null);
    if matches!(kind, "single" | "multi") && !options.as_array().is_some_and(|a| !a.is_empty()) {
        return bad(
            &format!("inquiry: a `{kind}` question needs `options`"),
            "options",
        );
    }
    let Some(re) = fantastic_kernel::current_task().map(|t| t.0) else {
        return json!({"error": "web_mcp: inquiry outside a task — nothing to answer under", "reason": "not_connected"});
    };
    let asker = fantastic_kernel::current_origin().map(|a| a.0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let request_id = p.get("request_id").cloned().unwrap_or(Value::Null);
    let held = json!({
        "re": re, "asker": asker, "text": text, "kind": kind, "options": options,
        "request_id": request_id,
        "reply_client_id": p.get("reply_client_id").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("cli"),
        "ts": ts,
    });
    let _g = HELD.lock().expect("inquiries poisoned");
    let mut list = open(kernel, leg);
    list.push(held);
    let n = list.len();
    store(kernel, leg, list);
    json!({"accepted": true, "re": re, "request_id": request_id, "open": n})
}

/// `inquiries`: the open ones, oldest first.
pub fn list(leg: &AgentId, p: &Value, kernel: &Arc<Kernel>) -> Value {
    let asker = p.get("asker").and_then(Value::as_str);
    let _g = HELD.lock().expect("inquiries poisoned");
    let mine: Vec<Value> = open(kernel, leg)
        .into_iter()
        .filter(|q| asker.is_none_or(|a| q["asker"].as_str() == Some(a)))
        .collect();
    json!({"inquiries": mine})
}

/// `answer`: the answer to the asker under the inquiry's pid; closed.
pub async fn answer(leg: &AgentId, p: &Value, kernel: &Arc<Kernel>) -> Value {
    let Some(re) = p.get("re").and_then(Value::as_u64) else {
        return bad("answer: `re` (the inquiry's pid) is required", "re");
    };
    let Some(text) = p
        .get("text")
        .or_else(|| p.get("answer"))
        .and_then(answer_text)
    else {
        return bad(
            "answer: `text` must be words, a list of picks, or true/false",
            "text",
        );
    };
    let (at, q) = {
        let _g = HELD.lock().expect("inquiries poisoned");
        let mut list = open(kernel, leg);
        let Some(at) = list.iter().position(|q| q["re"].as_u64() == Some(re)) else {
            return json!({"error": format!("web_mcp: answer: no open inquiry {re}"), "reason": "not_found", "arg": "re"});
        };
        let q = list.remove(at);
        store(kernel, leg, list);
        (at, q)
    };
    let Some(asker) = q["asker"].as_str().map(str::to_string) else {
        return json!({"error": "web_mcp: answer: the inquiry has no asker to answer", "reason": "not_found"});
    };
    let rid = q["request_id"].as_str().unwrap_or("");
    let body = format!(
        "[reply to {re} · {} inquiry]\n<answer>{text}</answer>\n<request_id>{rid}</request_id>",
        leg.as_str()
    );
    let client = q["reply_client_id"].as_str().unwrap_or("cli").to_string();
    let out = Arc::clone(kernel)
        .send(vec![Item::new(
            AgentId::from(asker.as_str()),
            json!({"type": "send", "text": body, "client_id": client}),
        )])
        .await
        .remove(0);
    match out.result.get("error").and_then(Value::as_str) {
        // the answer did not reach the asker: the question stays open
        Some(e) => {
            let _g = HELD.lock().expect("inquiries poisoned");
            let mut list = open(kernel, leg);
            list.insert(at.min(list.len()), q.clone());
            store(kernel, leg, list);
            json!({"error": format!("web_mcp: answer: the answer did not reach {asker}: {e}"), "reason": "refused"})
        }
        None => json!({"answered": re, "asker": asker}),
    }
}
