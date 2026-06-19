//! AI mode (M4a): drive a "brain" agent in the host kernel. The brain is an
//! ai-core backend agent (ollama / nvidia, env-selected); the product provisions
//! it lazily (a file_bridge for history + the backend agent) and drives a turn
//! with `send`. The universal `send` tool inside the agentic loop lets the brain
//! drive the kernel. v1 renders the FINAL response (live token streaming is a
//! follow-up). The Anthropic backend is the next sub-step (M4b).

use std::sync::Arc;

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

const BRAIN_ID: &str = "brain";
const FS_ID: &str = "ai_fs";

/// (handler_module, default model) for the selected backend.
/// `FANTASTIC_AI_BACKEND=ollama|nvidia` (default ollama — no key needed).
fn backend() -> (&'static str, String) {
    match std::env::var("FANTASTIC_AI_BACKEND").as_deref() {
        Ok("nvidia") => (
            "nvidia_nim_backend.tools",
            std::env::var("FANTASTIC_AI_MODEL")
                .unwrap_or_else(|_| "nvidia/llama-3_1-nemotron-ultra-253b-v1".to_string()),
        ),
        _ => (
            "ollama_backend.tools",
            std::env::var("FANTASTIC_AI_MODEL").unwrap_or_else(|_| "llama3.2".to_string()),
        ),
    }
}

fn err_of(v: &Value) -> Option<String> {
    v.get("error").and_then(Value::as_str).map(String::from)
}

/// Provision the brain once: an open file_bridge (history sink) + the backend
/// agent bound to it. Idempotent — a re-create returns the existing record.
async fn ensure_brain(kernel: &Arc<Kernel>) -> Result<String, String> {
    let probe = kernel
        .send(&AgentId::from(BRAIN_ID), json!({"type": "reflect"}))
        .await;
    if err_of(&probe).is_none() {
        return Ok(backend_label());
    }
    let fs = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"file_bridge.tools","id":FS_ID,"root":".","ingress_rule":"allow_all"}),
        )
        .await;
    if let Some(e) = err_of(&fs) {
        return Err(format!("file_bridge: {e}"));
    }
    let (handler, model) = backend();
    let brain = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":handler,"id":BRAIN_ID,"model":model,"file_bridge_id":FS_ID}),
        )
        .await;
    if let Some(e) = err_of(&brain) {
        return Err(format!("brain: {e}"));
    }
    Ok(backend_label())
}

fn backend_label() -> String {
    let (h, m) = backend();
    format!("{} · {}", h.trim_end_matches(".tools"), m)
}

/// Run one user turn through the brain; `tx` receives the rendered result.
pub async fn run_turn(kernel: Arc<Kernel>, text: String, tx: UnboundedSender<String>) {
    if let Err(e) = ensure_brain(&kernel).await {
        let _ = tx.send(format!("✗ {e}"));
        return;
    }
    let reply = kernel
        .send(
            &AgentId::from(BRAIN_ID),
            json!({"type":"send","text":text,"client_id":"fantastic"}),
        )
        .await;
    let out = reply
        .get("response")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| err_of(&reply).map(|e| format!("✗ {e}")))
        .unwrap_or_else(|| serde_json::to_string(&reply).unwrap_or_default());
    let _ = tx.send(out);
}
