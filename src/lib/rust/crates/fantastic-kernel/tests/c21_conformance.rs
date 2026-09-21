//! C21 — one semantics, many transports, MANY RUNTIMES.
//!
//! This driver replays the SHARED corpus (`contract/conformance/cases.json` at the repo
//! root) against the rust kernel. The python, swift and ts drivers replay the
//! SAME file. Add a case there and every runtime is on the hook for it — a
//! wire-visible contract is decidable from outside, so asserting it once as
//! DATA beats asserting it four times in four languages.
//!
//! The corpus is JSON so every runtime reads it with what it already has (the
//! ts kernel ships zero runtime deps); JSON is also valid YAML, so a yaml
//! parser reads it unchanged.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, Item, Kernel, Reply};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// This runtime's root id. `$root` in the corpus means exactly this — parity
/// asserts the per-runtime value, never equality (python's root is
/// `kernel_state` — ours too, since C12).
const ROOT_ID: &str = "kernel_state";

/// The corpus path, resolved from this crate rather than the cwd so the test is
/// runnable from anywhere in the workspace.
fn corpus_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(5)
        .expect("repo root above src/lib/rust/crates/fantastic-kernel")
        .join("contract/conformance/cases.json")
}

struct C21Probe;

#[async_trait]
impl Bundle for C21Probe {
    fn name(&self) -> &str {
        "c21_echo"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "boot" | "shutdown" => Ok(None),
            // In this runtime the BUNDLE answers `reflect` and the kernel
            // merges its own fields around the reply (python and ts answer it
            // in the kernel instead). The observable reply is what the corpus
            // grades, so the probe supplies the same identity either way.
            "reflect" => Ok(Some(json!({
                "id": _id.as_str(),
                "sentence": "C21 probe — answers `poke` identically on every transport.",
                "verbs": {"poke": "args: value:any — echoes {poked: value}"},
            }))),
            "poke" => Ok(Some(
                json!({ "poked": payload.get("value").cloned().unwrap_or(Value::Null) }),
            )),
            // Anything else — an EMPTY payload included — is `unhandled`, never
            // a silent null. Same agent as the python probe, by construction.
            other => Ok(Some(json!({
                "error": format!("c21_probe has no verb {other:?}"),
                "reason": "unhandled",
                "verb": other,
            }))),
        }
    }
}

fn subst_root(v: &Value) -> Value {
    match v {
        Value::String(s) if s == "$root" => json!(ROOT_ID),
        Value::Array(a) => Value::Array(a.iter().map(subst_root).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, x)| (k.clone(), subst_root(x))).collect())
        }
        other => other.clone(),
    }
}

/// Deep subset: the corpus asserts what MUST be present, never the whole reply.
fn subset(expected: &Value, actual: &Value) -> bool {
    match expected {
        Value::Object(e) => match actual {
            Value::Object(a) => e
                .iter()
                .all(|(k, v)| a.get(k).map(|av| subset(v, av)).unwrap_or(false)),
            _ => false,
        },
        other => other == actual,
    }
}

async fn wired() -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register("c21_echo.tools", C21Probe);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from(ROOT_ID),
        None,
        None,
        Map::new(),
        std::env::temp_dir().join("c21-rust"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(ROOT_ID),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": "c21_echo.tools",
                "id": "c21_probe",
            }),
        )])
        .await;
    kernel
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shared_c21_corpus_holds_on_this_runtime() {
    let raw = std::fs::read_to_string(corpus_path())
        .unwrap_or_else(|e| panic!("shared corpus unreadable at {:?}: {e}", corpus_path()));
    let doc: Value = serde_json::from_str(&raw).expect("corpus is not valid JSON");
    let cases = doc["cases"].as_array().expect("corpus has no `cases`");
    assert!(!cases.is_empty(), "corpus loaded no cases");

    let kernel = wired().await;
    let mut checked = 0usize;
    for c in cases {
        let name = c["name"].as_str().unwrap_or("<unnamed>");
        let target = c["target"].as_str().expect("case has no target");
        let payload = c["payload"].clone();
        let reply = kernel
            .send(vec![Item::new(AgentId::from(target), payload)])
            .await
            .remove(0)
            .result;

        if let Some(expect) = c.get("expect") {
            let want = subst_root(expect);
            assert!(
                subset(&want, &reply),
                "{name}: expected superset of {want}, got {reply}"
            );
        }
        for key in c
            .get("expect_keys")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            let k = key.as_str().unwrap_or("");
            assert!(
                reply.get(k).is_some(),
                "{name}: reply missing key {k:?}: {reply}"
            );
        }
        checked += 1;
    }
    assert_eq!(checked, cases.len(), "not every case was replayed");
}
