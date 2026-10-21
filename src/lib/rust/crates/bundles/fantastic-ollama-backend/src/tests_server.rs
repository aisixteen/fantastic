//! A `model` the record leaves out is READ FROM THE SERVER — python
//! `tests/test_ollama_server_read.py`, case for case.

use super::server::{self, served_model};
use super::*;
use fantastic_ai_core::credentials::CredentialError;
use fantastic_kernel::{Agent, Item};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tags(names: &[&str]) -> String {
    let models: Vec<Value> = names
        .iter()
        .map(|n| json!({"name": n, "model": n, "size": 1}))
        .collect();
    json!({ "models": models }).to_string()
}

async fn ollama(body: String, status: u16) -> MockServer {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&srv)
        .await;
    srv
}

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

pub(crate) struct NoKey;
#[async_trait]
impl KeySource for NoKey {
    async fn key(&self, _e: &str) -> Result<Option<String>, CredentialError> {
        Ok(None)
    }
}

struct Key;
#[async_trait]
impl KeySource for Key {
    async fn key(&self, _e: &str) -> Result<Option<String>, CredentialError> {
        Ok(Some("sekret".into()))
    }
}

#[tokio::test]
async fn the_one_served_model_is_read() {
    let srv = ollama(tags(&["qwen3:8b"]), 200).await;
    assert_eq!(
        server::read_model(&srv.uri(), &NoKey).await,
        Ok("qwen3:8b".to_string())
    );
    assert_eq!(seen(&srv).await, vec![("/api/tags".to_string(), None)]);
}

#[test]
fn a_server_without_one_model_refuses_naming_what_it_serves() {
    for (body, words) in [
        (
            tags(&["a:1", "b:2"]),
            "serves a:1, b:2 — not one model; set model on the record",
        ),
        (
            tags(&[]),
            "serves nothing — not one model; set model on the record",
        ),
        ("<html>".to_string(), "is not a model list"),
    ] {
        let e = served_model(&body, "u").unwrap_err();
        assert!(e.contains(words), "{e}");
    }
}

#[tokio::test]
async fn a_server_that_does_not_answer_refuses_naming_the_url() {
    let srv = ollama("nope".into(), 404).await;
    assert_eq!(
        server::read_model(&srv.uri(), &NoKey).await,
        Err(format!(
            "model: GET {}/api/tags answered HTTP 404 — the server (or a door in front of it) \
             does not serve it; set model on the record",
            srv.uri()
        ))
    );
    assert_eq!(
        server::read_model("http://127.0.0.1:1", &NoKey).await,
        Err(
            "model: GET http://127.0.0.1:1/api/tags failed — the server is not reachable \
             there; set model on the record"
                .to_string()
        )
    );
}

#[tokio::test]
async fn the_key_rides_the_model_list_when_the_record_names_one() {
    let srv = ollama(tags(&["m"]), 200).await;
    server::read_model(&srv.uri(), &Key).await.expect("read");
    assert_eq!(seen(&srv).await[0].1.as_deref(), Some("Bearer sekret"));
}

#[test]
fn a_record_with_only_endpoint_and_a_store_is_accepted() {
    let b = OllamaBackendBundle;
    let ok = json!({"endpoint": "http://o.test", "memory_store_id": "memory"});
    assert_eq!(b.validate_meta(ok.as_object().unwrap(), &Map::new()), None);
    let fields = record_schema()["fields"].as_array().unwrap();
    let model = fields.iter().find(|f| f["name"] == "model").unwrap();
    assert_eq!(model["required"], json!(false));
    let cred = fields.iter().find(|f| f["name"] == "credentials").unwrap();
    assert_eq!(cred["required"], json!(false));
}

// ─── through the kernel ─────────────────────────────────────────────────────

async fn mk(tmp: &TempDir, endpoint: &str) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, OllamaBackendBundle);
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
    let r = call(
        &kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
               "id": "o1", "endpoint": endpoint, "memory_store_id": "memory"}),
    )
    .await;
    assert_eq!(r["id"], json!("o1"), "{r}");
    kernel
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_reads_the_model_first() {
    let tmp = TempDir::new().unwrap();
    let srv = ollama(tags(&["qwen3:8b"]), 200).await;
    let kernel = mk(&tmp, &srv.uri()).await;
    call(
        &kernel,
        "o1",
        json!({"type": "send", "text": "hi", "client_id": "t"}),
    )
    .await;
    let rec = helpers::agent_meta(&AgentId::from("o1"), &kernel);
    assert_eq!(rec["model"], json!("qwen3:8b"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_the_server_cannot_answer_is_refused() {
    let tmp = TempDir::new().unwrap();
    let srv = ollama("nope".into(), 404).await;
    let kernel = mk(&tmp, &srv.uri()).await;
    let r = call(
        &kernel,
        "o1",
        json!({"type": "send", "text": "hi", "client_id": "t"}),
    )
    .await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("bad_arg"), &json!("model")),
        "{r}"
    );
    let rec = helpers::agent_meta(&AgentId::from("o1"), &kernel);
    assert!(rec.get("model").is_none_or(Value::is_null));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verb_that_runs_no_turn_reads_nothing() {
    let tmp = TempDir::new().unwrap();
    let srv = ollama(tags(&["m"]), 200).await;
    let kernel = mk(&tmp, &srv.uri()).await;
    for verb in ["reflect", "status", "context_status", "rows"] {
        call(&kernel, "o1", json!({"type": verb})).await;
    }
    assert!(seen(&srv).await.is_empty());
}

#[tokio::test]
async fn the_key_rides_the_chat_only_when_set() {
    let srv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("{\"message\":{\"content\":\"ok\"},\"done\":true}\n"),
        )
        .mount(&srv)
        .await;
    for (key, want) in [
        (Arc::new(Key) as Arc<dyn KeySource>, Some("Bearer sekret")),
        (Arc::new(NoKey) as Arc<dyn KeySource>, None),
    ] {
        let p = OllamaProvider {
            endpoint: srv.uri(),
            model: "m".into(),
            temperature: 0.3,
            num_ctx: None,
            num_predict: None,
            think: None,
            key,
            report: Default::default(),
            stream_idle: std::time::Duration::from_secs_f64(fantastic_ai_core::constants::STREAM_IDLE_SECONDS),
        };
        let mut stream = p
            .chat(&[json!({"role": "user", "content": "hi"})], None)
            .await
            .expect("chat");
        while futures_util::StreamExt::next(&mut stream).await.is_some() {}
        let last = seen(&srv).await.pop().expect("a request");
        assert_eq!(last.1.as_deref(), want);
    }
}
