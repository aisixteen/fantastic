//! What a record leaves out is READ FROM THE SERVER — python
//! `tests/test_server_read.py`, case for case. The bodies are the shapes a live
//! vLLM answers with.

use super::server::{self, kv_slots, metrics_url, served_model};
use super::*;
use fantastic_ai_core::credentials::CredentialError;
use fantastic_kernel::{Agent, Item};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "qwen3.8-27b";

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().cloned().expect("an object")
}

fn models(entries: &[(&str, i64)]) -> String {
    let data: Vec<Value> = entries
        .iter()
        .map(|(id, n)| json!({"id": id, "object": "model", "max_model_len": n}))
        .collect();
    json!({"object": "list", "data": data}).to_string()
}

fn metrics(concurrency: &[&str]) -> String {
    let mut lines =
        vec!["# HELP vllm:cache_config_info Information of the LLMEngine CacheConfig".to_string()];
    for c in concurrency {
        lines.push(format!(
            "vllm:cache_config_info{{block_size=\"432\",engine=\"0\",\
             kv_cache_max_concurrency=\"{c}\",kv_cache_size_tokens=\"74561\",\
             model_name=\"qwen3.8-27b\"}} 1.0"
        ));
    }
    lines.push("vllm:num_requests_running{engine=\"0\",model_name=\"qwen3.8-27b\"} 0.0".into());
    lines.join("\n") + "\n"
}

/// A vLLM that answers `/v1/models` and `/metrics` under its own path prefix.
async fn vllm(
    models_body: String,
    metrics_body: String,
    metrics_status: u16,
) -> (MockServer, String) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/s{n}/v1/models")))
        .respond_with(ResponseTemplate::new(200).set_body_string(models_body))
        .mount(&srv)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/s{n}/metrics")))
        .respond_with(ResponseTemplate::new(metrics_status).set_body_string(metrics_body))
        .mount(&srv)
        .await;
    let endpoint = format!("{}/s{n}/v1", srv.uri());
    (srv, endpoint)
}

async fn default_vllm() -> (MockServer, String) {
    vllm(
        models(&[(MODEL, 65536)]),
        metrics(&["1.1377245508982037"]),
        200,
    )
    .await
}

/// (path, authorization) of every request the server saw.
async fn seen(srv: &MockServer) -> Vec<(String, Option<String>)> {
    srv.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            (
                r.url.path().to_string(),
                r.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
            )
        })
        .collect()
}

struct NoKey;

#[async_trait]
impl KeySource for NoKey {
    async fn key(&self, _endpoint: &str) -> Result<Option<String>, CredentialError> {
        Ok(None)
    }
}

struct Key;

#[async_trait]
impl KeySource for Key {
    async fn key(&self, _endpoint: &str) -> Result<Option<String>, CredentialError> {
        Ok(Some("sekret".into()))
    }
}

fn rec(endpoint: &str, extra: Value) -> Map<String, Value> {
    let mut r = obj(json!({
        "endpoint": endpoint, "kv_overflow": "evict", "memory_store_id": "memory",
    }));
    r.extend(obj(extra));
    r
}

// ─── the read ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn what_the_record_leaves_out_is_read() {
    let (srv, ep) = default_vllm().await;
    let got = server::read(&rec(&ep, json!({})), &NoKey)
        .await
        .expect("read");
    assert_eq!(
        Value::Object(got),
        json!({"model": MODEL, "max_tokens": 65536, "context_window": 65536, "kv_slots": 1})
    );
    let paths: Vec<String> = seen(&srv).await.into_iter().map(|(p, _)| p).collect();
    assert!(
        paths[0].ends_with("/v1/models") && paths[1].ends_with("/metrics"),
        "{paths:?}"
    );
}

#[tokio::test]
async fn a_field_the_record_sets_is_never_read() {
    let (srv, ep) = default_vllm().await;
    let full =
        json!({"model": MODEL, "max_tokens": 8000, "context_window": 262_144, "kv_slots": 6});
    let got = server::read(&rec(&ep, full), &NoKey).await.expect("read");
    assert!(got.is_empty());
    assert!(seen(&srv).await.is_empty());
    let partial = json!({"model": MODEL, "max_tokens": 8000, "context_window": 262_144});
    let got = server::read(&rec(&ep, partial), &NoKey)
        .await
        .expect("read");
    assert_eq!(Value::Object(got), json!({"kv_slots": 1}));
    let paths: Vec<String> = seen(&srv).await.into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("/metrics"));
}

#[test]
fn the_record_s_model_picks_its_entry_among_several() {
    let body = models(&[("other", 4096), (MODEL, 262_144)]);
    let e = served_model(&body, Some(MODEL), "u", "max_tokens").expect("entry");
    assert_eq!(e["max_model_len"], json!(262_144));
}

#[test]
fn a_llama_cpp_entry_gives_its_window_as_meta_n_ctx() {
    let e = obj(json!({"id": "q", "object": "model", "meta": {"n_ctx": 131_072}}));
    assert_eq!(
        server::max_model_len(&e, "u", "context_window").expect("n_ctx"),
        131_072
    );
    let none = obj(json!({"id": "q", "meta": {}}));
    let err = server::max_model_len(&none, "u", "context_window").unwrap_err();
    assert_eq!(err.field, "context_window");
}

#[test]
fn several_served_models_and_no_model_refuse_naming_them() {
    let body = models(&[("a", 4096), ("b", 8192)]);
    let e = served_model(&body, None, "http://h/v1/models", "model").unwrap_err();
    assert_eq!(e.field, "model");
    assert_eq!(
        e.message,
        "model: http://h/v1/models serves a, b — not one model; set model on the record"
    );
}

#[test]
fn a_model_the_server_does_not_serve_is_refused() {
    let e = served_model(&models(&[(MODEL, 1)]), Some("nope"), "u", "max_tokens").unwrap_err();
    assert_eq!(e.field, "max_tokens");
    assert_eq!(
        e.message,
        format!("max_tokens: u does not serve model 'nope' — it serves {MODEL}")
    );
}

#[tokio::test]
async fn a_door_without_metrics_refuses_naming_kv_slots() {
    let (_srv, ep) = vllm(models(&[(MODEL, 1)]), "not found".into(), 404).await;
    let r = rec(
        &ep,
        json!({"model": MODEL, "max_tokens": 8000, "context_window": 262_144}),
    );
    let e = server::read(&r, &NoKey).await.unwrap_err();
    let url = ep.trim_end_matches("/v1").to_string() + "/metrics";
    assert_eq!(e.field, "kv_slots");
    assert_eq!(
        e.message,
        format!(
            "kv_slots: GET {url} answered HTTP 404 — the server (or a door in front of it) \
             does not serve it; set kv_slots on the record"
        )
    );
}

#[tokio::test]
async fn an_unreachable_server_refuses_naming_the_field() {
    let e = server::read(&rec("http://127.0.0.1:1/v1", json!({})), &NoKey)
        .await
        .unwrap_err();
    assert_eq!(e.field, "model");
    assert_eq!(
        e.message,
        "model: GET http://127.0.0.1:1/v1/models failed — the server is not reachable \
         there; set model on the record"
    );
}

#[test]
fn kv_slots_is_the_floor_of_vllm_s_concurrency() {
    for (c, want) in [("1.1377245508982037", 1), ("6.9", 6), ("3", 3)] {
        assert_eq!(kv_slots(&metrics(&[c]), "u"), Ok(want), "{c}");
    }
}

#[test]
fn a_metrics_page_that_names_no_count_refuses() {
    for (body, words) in [
        (
            metrics(&["0.6"]),
            "holds 0.6 full-length conversations, under one",
        ),
        (
            metrics(&["2.0", "3.0"]),
            "carries 2 vllm:cache_config_info lines, not one",
        ),
        (
            metrics(&[]),
            "carries 0 vllm:cache_config_info lines, not one",
        ),
        (
            "vllm:cache_config_info{block_size=\"432\"} 1.0\n".to_string(),
            "names no kv_cache_max_concurrency",
        ),
    ] {
        let e = kv_slots(&body, "u").unwrap_err();
        assert_eq!(e.field, "kv_slots");
        assert!(e.message.contains(words), "{}", e.message);
    }
}

#[tokio::test]
async fn metrics_url_on_the_record_wins_and_the_key_stays_on_the_endpoint_host() {
    let (srv, ep) = default_vllm().await;
    let (msrv, mep) = default_vllm().await;
    // a metrics page on ANOTHER host (another port is another host)
    let murl = mep.trim_end_matches("/v1").to_string() + "/metrics";
    server::read(&rec(&ep, json!({"metrics_url": murl})), &Key)
        .await
        .expect("read");
    assert_eq!(seen(&srv).await[0].1.as_deref(), Some("Bearer sekret"));
    assert_eq!(seen(&msrv).await[0].1, None);
    // the same host gets the key
    let r = rec(
        &ep,
        json!({"model": MODEL, "max_tokens": 1, "context_window": 2}),
    );
    server::read(&r, &Key).await.expect("read");
    let last = seen(&srv).await.pop().expect("a request");
    assert!(last.0.ends_with("/metrics") && last.1.as_deref() == Some("Bearer sekret"));
}

#[test]
fn metrics_url_is_the_endpoint_s_v1_replaced_by_metrics() {
    let m = |v: Value| metrics_url(&obj(v));
    assert_eq!(
        m(json!({"endpoint": "http://h:10000/v1"})).as_deref(),
        Some("http://h:10000/metrics")
    );
    assert_eq!(
        m(json!({"endpoint": "https://d.net/llm/openai/v1/"})).as_deref(),
        Some("https://d.net/llm/openai/metrics")
    );
    assert_eq!(m(json!({"endpoint": "http://h:10000/api"})), None);
    assert_eq!(
        m(json!({"endpoint": "http://h/api", "metrics_url": "http://m/x"})).as_deref(),
        Some("http://m/x")
    );
}

// ─── the write ──────────────────────────────────────────────────────────────

#[test]
fn a_record_with_only_endpoint_and_kv_overflow_is_accepted() {
    assert_eq!(
        validate(&rec("http://vllm.test/v1", json!({})), &Map::new()),
        None
    );
}

#[test]
fn a_write_the_server_read_cannot_honour_is_refused() {
    let base = rec("http://vllm.test/v1", json!({}));
    for (meta, fragment) in [
        (
            json!({"context_window": "big"}),
            "context_window must be a positive int",
        ),
        (
            json!({"context_window": 0}),
            "context_window must be a positive int",
        ),
        (
            json!({"metrics_url": "vllm.test/metrics"}),
            "metrics_url must be an http(s) URL",
        ),
        (
            json!({"metrics_url": 9100}),
            "metrics_url must be an http(s) URL",
        ),
        (
            json!({"endpoint": "http://h:10000/api"}),
            "kv_slots is left to the server, but the endpoint 'http://h:10000/api' does \
             not end in /v1",
        ),
    ] {
        let err = validate(&obj(meta.clone()), &base).unwrap_or_default();
        assert!(err.contains(fragment), "{meta}: {err}");
    }
}

#[test]
fn an_endpoint_without_v1_passes_with_metrics_url_or_kv_slots() {
    let base = rec("http://h:10000/api", json!({}));
    assert_eq!(
        validate(
            &obj(json!({"metrics_url": "http://h:10000/metrics"})),
            &base
        ),
        None
    );
    assert_eq!(validate(&obj(json!({"kv_slots": 3})), &base), None);
}

#[test]
fn caching_none_and_cleanup_are_accepted_and_provider_cache_is_refused_here() {
    // vllm declares no provider caching: the mode is refused at the write, naming
    // the backend; the two modes that send no marker are accepted.
    let rec = rec("http://vllm.test/v1", json!({}));
    for mode in ["none", "cleanup"] {
        let meta = json!({"caching": mode});
        assert_eq!(validate(&obj(meta.clone()), &rec), None, "{mode}");
    }
    let meta = json!({"caching": "provider_cache"});
    assert_eq!(
        validate(&obj(meta.clone()), &rec).as_deref(),
        Some(
            "vllm_backend cannot do provider caching: caching=provider_cache is refused — use \
             none or cleanup"
        )
    );
    // a record already in provider_cache is refused on any write to it
    let stored = json!({
        "endpoint": "http://vllm.test/v1", "kv_overflow": "evict",
        "memory_store_id": "memory", "caching": "provider_cache"
    });
    assert!(validate(&Map::new(), stored.as_object().unwrap()).is_some());
    // an unknown mode is the schema's enum refusal
    let meta = json!({"caching": "always"});
    assert_eq!(
        validate(&obj(meta.clone()), &rec).as_deref(),
        Some("caching='always' is not one of the legal values: none, cleanup, provider_cache")
    );
}

// ─── through the kernel ─────────────────────────────────────────────────────

async fn mk_kernel(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, VllmBundle);
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
    kernel
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn create(kernel: &Arc<Kernel>, meta: Map<String, Value>) -> Value {
    let mut payload = json!({
        "type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
    });
    for (k, v) in meta {
        payload[k] = v;
    }
    call(kernel, "kernel_state", payload).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_reads_the_server_first_and_reflect_shows_the_values() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let (_srv, ep) = default_vllm().await;
    let mut meta = rec(&ep, json!({}));
    meta.insert("id".into(), json!("v1"));
    let r = create(&kernel, meta).await;
    assert_eq!(r["id"], json!("v1"), "{r}");
    let before = call(&kernel, "v1", json!({"type": "reflect"})).await;
    assert_eq!(before["max_tokens"], Value::Null, "{before}");
    call(
        &kernel,
        "v1",
        json!({"type": "send", "text": "hi", "client_id": "t"}),
    )
    .await;
    let rec = helpers::agent_meta(&AgentId::from("v1"), &kernel);
    assert_eq!(
        (
            &rec["model"],
            &rec["max_tokens"],
            &rec["context_window"],
            &rec["kv_slots"]
        ),
        (&json!(MODEL), &json!(65536), &json!(65536), &json!(1)),
        "{rec:?}"
    );
    let after = call(&kernel, "v1", json!({"type": "reflect"})).await;
    assert_eq!(after["max_tokens"], json!(65536), "{after}");
    assert_eq!(after["kv"]["slots"], json!(1), "{after}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_the_server_cannot_answer_is_refused_naming_the_field() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let (srv, ep) = vllm(models(&[(MODEL, 1)]), String::new(), 404).await;
    let mut meta = rec(
        &ep,
        json!({"model": MODEL, "max_tokens": 8000, "context_window": 8192}),
    );
    meta.insert("id".into(), json!("v1"));
    create(&kernel, meta).await;
    let r = call(
        &kernel,
        "v1",
        json!({"type": "send", "text": "hi", "client_id": "t"}),
    )
    .await;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    assert_eq!(r["arg"], json!("kv_slots"), "{r}");
    assert!(
        r["error"]
            .as_str()
            .is_some_and(|e| e.contains("answered HTTP 404")),
        "{r}"
    );
    let rec = helpers::agent_meta(&AgentId::from("v1"), &kernel);
    assert!(rec.get("kv_slots").is_none_or(Value::is_null));
    assert!(!seen(&srv)
        .await
        .iter()
        .any(|(p, _)| p.ends_with("/chat/completions")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verb_that_runs_no_turn_reads_nothing() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let (srv, ep) = default_vllm().await;
    let mut meta = rec(&ep, json!({}));
    meta.insert("id".into(), json!("v1"));
    create(&kernel, meta).await;
    for verb in ["reflect", "status", "context_status", "rows"] {
        call(&kernel, "v1", json!({"type": verb})).await;
    }
    assert!(seen(&srv).await.is_empty());
}

struct Refused;

#[async_trait]
impl KeySource for Refused {
    async fn key(&self, _endpoint: &str) -> Result<Option<String>, CredentialError> {
        Err(CredentialError(
            "vllm_backend: the store refused the key".into(),
        ))
    }
}

#[tokio::test]
async fn a_key_the_store_refuses_refuses_naming_the_first_field() {
    let (_srv, ep) = default_vllm().await;
    let e = server::read(&rec(&ep, json!({"model": MODEL})), &Refused)
        .await
        .unwrap_err();
    assert_eq!(e.field, "max_tokens");
    assert_eq!(e.message, "vllm_backend: the store refused the key");
}
