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

/// Pure core of [`backend`]: pick `(handler_module, model)` from the selected
/// backend + optional model override. Extracted so it's unit-testable without
/// mutating process env.
fn backend_for(backend: Option<&str>, model: Option<&str>) -> (&'static str, String) {
    match backend {
        Some("nvidia") => (
            "nvidia_nim_backend.tools",
            model
                .map(String::from)
                .unwrap_or_else(|| "nvidia/llama-3_1-nemotron-ultra-253b-v1".to_string()),
        ),
        Some("anthropic") => (
            fantastic_anthropic_backend::HANDLER_MODULE,
            model
                .map(String::from)
                .unwrap_or_else(|| fantastic_anthropic_backend::DEFAULT_MODEL.to_string()),
        ),
        _ => (
            "ollama_backend.tools",
            model
                .map(String::from)
                .unwrap_or_else(|| "llama3.2".to_string()),
        ),
    }
}

/// (handler_module, default model) for the selected backend.
/// `FANTASTIC_AI_BACKEND=ollama|nvidia|anthropic` (default ollama — no key needed).
fn backend() -> (&'static str, String) {
    let b = std::env::var("FANTASTIC_AI_BACKEND").ok();
    let m = std::env::var("FANTASTIC_AI_MODEL").ok();
    backend_for(b.as_deref(), m.as_deref())
}

fn err_of(v: &Value) -> Option<String> {
    v.get("error").and_then(Value::as_str).map(String::from)
}

/// Provision the brain once: an open file_bridge (history sink) + the backend
/// agent bound to it. Idempotent — a re-create returns the existing record.
/// Returns the backend label (handler · model) for display.
pub async fn ensure_brain(kernel: &Arc<Kernel>) -> Result<String, String> {
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
    // ollama's default context (4096) is too small for the rebuilt-every-turn
    // system block (primer + reflect + agent menu + howto) of a full host. Set a
    // roomier window; `FANTASTIC_NUM_CTX` overrides. Cloud backends ignore it.
    let num_ctx: u64 = std::env::var("FANTASTIC_NUM_CTX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16384);
    let brain = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":handler,"id":BRAIN_ID,"model":model,"file_bridge_id":FS_ID,"num_ctx":num_ctx}),
        )
        .await;
    if let Some(e) = err_of(&brain) {
        return Err(format!("brain: {e}"));
    }
    // Key-requiring backends (nvidia/anthropic): provision the api_key from the
    // environment if present. No fallback — if the key is absent we leave it
    // unset and the first `send` returns a clean "api_key not set" error in the
    // pane. ollama needs no key.
    if let Some(key) = api_key_from_env() {
        let r = kernel
            .send(
                &AgentId::from(BRAIN_ID),
                json!({"type":"set_api_key","api_key":key}),
            )
            .await;
        if let Some(e) = err_of(&r) {
            return Err(format!("set_api_key: {e}"));
        }
    }
    Ok(backend_label())
}

/// Pure core of [`api_key_from_env`]: the generic key wins (if non-blank), else
/// the provider-conventional key for key-requiring backends; `None` for ollama
/// or when nothing usable is set. Blank/whitespace values are ignored.
fn api_key_for(
    generic: Option<&str>,
    backend: Option<&str>,
    anthropic: Option<&str>,
    nvidia: Option<&str>,
) -> Option<String> {
    if let Some(k) = generic {
        if !k.trim().is_empty() {
            return Some(k.to_string());
        }
    }
    let key = match backend {
        Some("anthropic") => anthropic,
        Some("nvidia") => nvidia,
        _ => return None,
    };
    key.filter(|k| !k.trim().is_empty()).map(String::from)
}

/// The api_key for the selected backend, read from the environment. A generic
/// `FANTASTIC_AI_KEY` wins; otherwise the provider-conventional var. `None` for
/// ollama (no key) or when nothing is set.
fn api_key_from_env() -> Option<String> {
    let generic = std::env::var("FANTASTIC_AI_KEY").ok();
    let backend = std::env::var("FANTASTIC_AI_BACKEND").ok();
    let anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
    let nvidia = std::env::var("NVIDIA_API_KEY").ok();
    api_key_for(
        generic.as_deref(),
        backend.as_deref(),
        anthropic.as_deref(),
        nvidia.as_deref(),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_defaults_to_ollama() {
        assert_eq!(
            backend_for(None, None),
            ("ollama_backend.tools", "llama3.2".to_string())
        );
        // an unknown backend also falls back to ollama.
        assert_eq!(backend_for(Some("bogus"), None).0, "ollama_backend.tools");
    }

    #[test]
    fn backend_selects_provider_and_honors_model_override() {
        assert_eq!(
            backend_for(Some("nvidia"), None).0,
            "nvidia_nim_backend.tools"
        );
        assert_eq!(
            backend_for(Some("anthropic"), None).0,
            fantastic_anthropic_backend::HANDLER_MODULE
        );
        assert_eq!(
            backend_for(Some("anthropic"), None).1,
            fantastic_anthropic_backend::DEFAULT_MODEL
        );
        // an explicit model overrides the per-backend default, for any backend.
        assert_eq!(
            backend_for(Some("ollama"), Some("gemma4:12b")).1,
            "gemma4:12b"
        );
        assert_eq!(backend_for(Some("nvidia"), Some("x")).1, "x");
    }

    #[test]
    fn api_key_generic_wins_when_present() {
        assert_eq!(
            api_key_for(Some("g"), Some("anthropic"), Some("a"), None).as_deref(),
            Some("g")
        );
    }

    #[test]
    fn api_key_falls_back_to_provider_specific() {
        assert_eq!(
            api_key_for(None, Some("anthropic"), Some("a"), None).as_deref(),
            Some("a")
        );
        assert_eq!(
            api_key_for(None, Some("nvidia"), None, Some("n")).as_deref(),
            Some("n")
        );
    }

    #[test]
    fn api_key_none_for_ollama_and_ignores_blanks() {
        // ollama (default / explicit) never carries a key, even if provider vars are set.
        assert_eq!(api_key_for(None, None, Some("a"), Some("n")), None);
        assert_eq!(api_key_for(None, Some("ollama"), Some("a"), None), None);
        // a blank generic key is ignored → falls through to the provider key.
        assert_eq!(
            api_key_for(Some("  "), Some("anthropic"), Some("a"), None).as_deref(),
            Some("a")
        );
        // a blank provider key yields None (no fallback).
        assert_eq!(
            api_key_for(None, Some("anthropic"), Some("   "), None),
            None
        );
    }

    #[test]
    fn err_of_extracts_error_string() {
        assert_eq!(err_of(&json!({"error": "boom"})).as_deref(), Some("boom"));
        assert_eq!(err_of(&json!({"ok": true})), None);
    }
}
