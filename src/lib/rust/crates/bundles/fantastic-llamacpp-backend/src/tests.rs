//! Unit tests for `fantastic-llamacpp-backend`.
//!
//! This bundle had none until the python-parity pass; what it answers `reflect`
//! with is what every model on a local llama-server reads about itself, so the
//! reflect body is pinned here against `llamacpp_backend/tools.py`.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::Map;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Per-test id derived from the tempdir name — the state maps in this crate are
/// process-global, so a shared id would race across parallel tests.
fn agent_id_for(tmp: &TempDir) -> String {
    format!(
        "lc_{}",
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

/// Kernel + root + this bundle's agent. No file leg: the conversation lives in
/// the agent's memory store (a `ram_store`, wired both ways).
async fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, LlamaCppBundle);
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let id = agent_id_for(tmp);
    let memory =
        fantastic_ram_store::test_support::memory_meta(&kernel, &id, json!({ "cli": "*" })).await;
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": HANDLER_MODULE,
        "id": id,
    });
    for (k, v) in memory {
        create[k] = v;
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    (kernel, AgentId::from(id.as_str()))
}

#[test]
fn readme_is_python_verbatim() {
    // B4.5 — the readme a model reads about this backend is the PYTHON bundle's,
    // byte for byte. Each runtime had written its own, so the same agent
    // described itself differently depending on where it ran.
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/ai/llamacpp/llamacpp_backend/src/llamacpp_backend/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

#[test]
fn the_unset_model_is_pythons_sentinel() {
    // `llama-server` serves exactly ONE model and ignores the request's `model`
    // field, so there is nothing to guess; python names the same sentinel.
    assert_eq!(DEFAULT_MODEL, "(server-loaded)");
    let p = LlamaCppProvider {
        endpoint: DEFAULT_ENDPOINT.to_string(),
        requested_model: None,
        api_key: None,
        max_tokens: None,
        temperature: DEFAULT_TEMPERATURE,
        think: None,
        report: Default::default(),
    };
    assert_eq!(p.model(), "(server-loaded)");
}

/// B4.4 — python folds the default in at CONSTRUCTION
/// (`self._temperature = 0.3 if temperature is None else float(temperature)`),
/// so `temperature` is on the wire of EVERY request. Rust omitted the field
/// when the record named none, leaving the server's own default (often 0.8) to
/// decide how literal a tool-calling model was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temperature_is_always_on_the_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok<call name=\\\"done\\\"></call>\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(),
                   "endpoint": format!("{}/v1", server.uri())}),
        )])
        .await;
    let _ = kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await;
    let reqs = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["temperature"], json!(DEFAULT_TEMPERATURE));

    // …and the record still overrides it.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(),
                   "endpoint": format!("{}/v1", server.uri()), "temperature": 0.9}),
        )])
        .await;
    let _ = kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await;
    let reqs = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert_eq!(body["temperature"], json!(0.9));
}

#[tokio::test]
async fn reflect_extras_match_python() {
    // python `_make_reflect` + `llamacpp_backend/tools.py::_reflect_extra`: the
    // shared base body in python's order, then `temperature` (0.3 by default),
    // `num_predict`, `think`, `has_api_key`. `num_ctx` is deliberately ABSENT —
    // llama-server fixes the window at start — and so are the rust-only `note`
    // and `file_bridge_id` keys.
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(agent.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    let keys: Vec<String> = r.as_object().unwrap().keys().take(16).cloned().collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "sentence",
            "model",
            "endpoint",
            "context_window",
            "context_strategy",
            "memory_store_id",
            "verbs",
            "generating",
            "cancel",
            "emits",
            "concurrency",
            "temperature",
            "num_predict",
            "think",
            "has_api_key",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>(),
        "{r:#?}"
    );
    assert_eq!(
        r["sentence"],
        "llama.cpp-backed local LLM agent (raw prompt-and-parse tool-calling)."
    );
    assert_eq!(r["model"], "(server-loaded)");
    assert_eq!(r["temperature"], 0.3);
    assert_eq!(r["num_predict"], Value::Null);
    assert_eq!(r["think"], Value::Null);
    assert_eq!(r["has_api_key"], false);
    assert!(r.get("num_ctx").is_none(), "{r:#?}");
    assert!(r.get("note").is_none(), "{r:#?}");
    assert!(r.get("file_bridge_id").is_none(), "{r:#?}");
    // The shared verb menu and the shared emits, not a bundle-private copy.
    assert_eq!(r["verbs"], fantastic_ai_core::verbs::reflect_verbs());
    assert_eq!(r["emits"], fantastic_ai_core::verbs::reflect_emits());

    // Set the knobs: python's conversions apply.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(),
                   "temperature": 0.7, "num_predict": 0, "think": false,
                   "api_key": "k"}),
        )])
        .await;
    let r = kernel
        .send(vec![Item::new(agent, json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["temperature"], 0.7);
    assert_eq!(r["num_predict"], Value::Null, "0 is falsy → None");
    assert_eq!(r["think"], false, "present-and-not-null → bool(x)");
    assert_eq!(r["has_api_key"], true);
}

/// B3.1 — python `handler`'s `{name}: unknown type {t!r}`: python's repr (single
/// quotes), and `shutdown` is NOT in the VERBS map, so it is unknown like
/// anything else. The prefix is the BACKEND's name — rust answered `ollama:`
/// where python answers `ollama_backend:`.
#[tokio::test]
async fn unknown_type_is_pythons_repr_and_shutdown_is_unknown() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    for (verb, want) in [
        ("garbage", "llamacpp_backend: unknown type 'garbage'"),
        ("shutdown", "llamacpp_backend: unknown type 'shutdown'"),
    ] {
        let r = kernel
            .send(vec![Item::new(agent.clone(), json!({"type": verb}))])
            .await
            .remove(0)
            .result;
        assert_eq!(r["error"], json!(want));
    }
}
