//! `fantastic canvas run vllm -p PORT [--host HOST] [--kv-overflow evict|reject]`:
//! the canvas's ROOT AI — the AI its view's main chat (`main_ai_chat`) shows —
//! speaks to the vLLM server at `http://HOST:PORT/v1`.
//!
//! - a main chat whose AI is a vLLM agent: that AI is RE-POINTED (endpoint,
//!   kv_overflow) and what the server decides — model, context_window,
//!   max_tokens, kv_slots, metrics_url — is CLEARED, so vllm_backend reads it
//!   from the new server before its first turn (and writes it back);
//! - otherwise a new vLLM AI is made under `canvas_root` (on the project's
//!   memory store, as `spawn` makes one), and the main chat shows it: the
//!   existing main chat cell is re-pointed, or a chat cell is written on the view
//!   and named its main chat.
//!
//! The page's records are written through `web_loader` (the page reads them at
//! load). The port is the caller's, the host `127.0.0.1` unless named; the rest
//! is the server's.

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

pub const VLLM: &str = "vllm_backend.tools";
const CHAT: &str = "ai_view.ts";
const VIEW: &str = "view_root";
/// What the server decides; cleared on a re-point so the new server is read.
const FROM_SERVER: [&str; 5] = ["model", "context_window", "max_tokens", "kv_slots", "metrics_url"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VllmRun {
    pub host: String,
    pub port: u16,
    pub kv_overflow: String,
}

impl VllmRun {
    pub fn endpoint(&self) -> String {
        format!("http://{}:{}/v1", self.host, self.port)
    }
}

/// The chat cell's own grant onto the AI it shows — `ai_view.ts`'s `grants`.
fn chat_grants(ai: &str) -> Value {
    json!({ai: ["watch", "reflect", "history", "status", "send", "stop"], "kernel": ["ps", "cancel"],
           "canvas_root": ["keep", "rename"], "$parent": ["update"]})
}

fn err_of(r: &Value) -> Option<String> {
    r.get("error").and_then(Value::as_str).map(str::to_string)
}

async fn page_records(h: &Host) -> Result<Vec<Map<String, Value>>, String> {
    let r = h.operator(crate::kern::web::CANVAS_LOADER_ID, json!({"type": "load_tree"})).await.result;
    if let Some(e) = err_of(&r) {
        return Err(format!("the page's records: {e}"));
    }
    Ok(r.get("records").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_object().cloned()).collect())
}

async fn persist(h: &Host, rec: Map<String, Value>) -> Result<(), String> {
    let id = rec.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let r = h.operator(crate::kern::web::CANVAS_LOADER_ID, json!({"type": "persist_record", "record": rec})).await.result;
    err_of(&r).map_or(Ok(()), |e| Err(format!("writing the page record `{id}`: {e}")))
}

fn handler_of(h: &Host, id: &str) -> Option<String> {
    let a = h.kernel.agents.get(&fantastic_kernel::AgentId::from(id))?;
    a.value().record().handler_module
}

/// Point the canvas's root AI at the server; says what it did.
pub async fn run_vllm(h: &Host, v: &VllmRun) -> Result<String, String> {
    // a build without the backend would skip the AI it points at and make another
    if h.kernel.bundles.get(VLLM).is_none() {
        return Err(format!("this build has no vLLM backend (`{VLLM}`) — build with `--features vllm`"));
    }
    let records = page_records(h).await?;
    let find = |id: &str| records.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(id)).cloned();
    let mut view = find(VIEW).ok_or("the page has no view `view_root` yet — open the canvas once, then run again")?;
    let cell = view.get("main_ai_chat").and_then(Value::as_str).and_then(|c| find(c));
    let shown = cell.as_ref().and_then(|c| c.get("backend_id")).and_then(Value::as_str).map(str::to_string);

    // RE-POINT the AI the main chat already shows, when it is a vLLM one
    if let Some(ai) = shown.as_deref().filter(|ai| handler_of(h, ai).as_deref() == Some(VLLM)) {
        let mut up = Map::new();
        up.insert("type".into(), json!("update_agent"));
        up.insert("id".into(), json!(ai));
        up.insert("endpoint".into(), json!(v.endpoint()));
        up.insert("kv_overflow".into(), json!(v.kv_overflow));
        for f in FROM_SERVER {
            up.insert(f.into(), Value::Null);
        }
        let r = h.operator("kernel", Value::Object(up)).await.result;
        if let Some(e) = err_of(&r) {
            return Err(format!("re-pointing `{ai}`: {e}"));
        }
        return Ok(format!("the root AI `{ai}` now speaks to {}", v.endpoint()));
    }

    // a NEW vLLM AI under canvas_root
    let ai = format!("ai_{}", crate::kern::backend::hex8());
    let mut spec = Map::new();
    spec.insert("handler_module".into(), json!(VLLM));
    spec.insert("endpoint".into(), json!(v.endpoint()));
    spec.insert("kv_overflow".into(), json!(v.kv_overflow));
    let me = fantastic_kernel::AgentId::from(crate::kern::web::CANVAS_NODE_ID);
    crate::kern::backend::BackendBundle::make_ai(h, &me, &ai, spec, Map::new()).await?;

    // the main chat shows it: its cell re-pointed, or a new one named main
    let cell_id = match cell {
        Some(mut c) => {
            let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            c.insert("backend_id".into(), json!(ai));
            c.insert("backend_root".into(), json!(ai));
            let mut cc = c.get("can_call").and_then(Value::as_object).cloned().unwrap_or_default();
            if let Some(old) = shown.as_deref() {
                cc.remove(old);
            }
            if let Value::Object(g) = chat_grants(&ai) {
                cc.extend(g);
            }
            c.insert("can_call".into(), Value::Object(cc));
            persist(h, c).await?;
            id
        }
        None => {
            let id = format!("ai_view_{}", crate::kern::backend::hex8());
            let mut c = Map::new();
            for (k, val) in [
                ("id", json!(id)),
                ("handler_module", json!(CHAT)),
                ("parent_id", json!(VIEW)),
                ("owns_backend", json!(false)),
                ("backend_id", json!(ai)),
                ("backend_root", json!(ai)),
                // the place verb's own size for a new cell, at the view's origin
                ("x", json!(0)),
                ("y", json!(0)),
                ("width", json!(480)),
                ("height", json!(320)),
                ("can_call", chat_grants(&ai)),
                ("callable_by", json!({"$self": "*"})),
            ] {
                c.insert(k.into(), val);
            }
            persist(h, c).await?;
            view.insert("main_ai_chat".into(), json!(id));
            persist(h, view).await?;
            id
        }
    };
    Ok(format!("the root AI `{ai}` (main chat `{cell_id}`) speaks to {}", v.endpoint()))
}
