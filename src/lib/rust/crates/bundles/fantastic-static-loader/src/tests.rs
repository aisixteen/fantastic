//! static_loader — declarative YAML roster hydration + the deny-all closure.
//! Mirror of the python `static_loader/tests/test_static_loader.py`.

use super::*;
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{with_sender, Agent, AgentRecord};
use serde_json::{json, Map};
use std::io::Write;

/// A trivial roster agent — answers boot/reflect, nothing else.
struct MockBundle;

#[async_trait]
impl Bundle for MockBundle {
    fn name(&self) -> &str {
        "mock"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(Some(match verb {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({"id": agent_id.0, "sentence": "mock"}),
            _ => json!({"ok": true}),
        }))
    }
}

const MANIFEST: &str = r#"
agents:
  - id: memory
    handler_module: mock.tools
    mode: mem
  - id: worker
    handler_module: mock.tools
    can_call:
      memory: [set, read]
"#;

fn mk_kernel() -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register("mock.tools", MockBundle);
    kernel.bundles.register(HANDLER_MODULE, StaticLoaderBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    );
    let _ = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

/// Write `text` to a temp file (kept alive for the process) and return its path.
fn write_manifest(text: &str) -> String {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(text.as_bytes()).unwrap();
    let path = f.path().to_string_lossy().to_string();
    std::mem::forget(f); // keep the file on disk for the test's duration
    path
}

async fn boot_with_manifest(text: &str) -> Arc<Kernel> {
    let kernel = mk_kernel();
    let path = write_manifest(text);
    // Operator send (top-level → ungated); create_agent auto-boots the loader,
    // which hydrates the roster.
    let rec = kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": "static_loader",
                "manifest": path,
            }),
        )
        .await;
    assert_eq!(rec["id"], json!("static_loader"), "loader create: {rec}");
    kernel
}

fn record(kernel: &Arc<Kernel>, id: &str) -> AgentRecord {
    kernel.agents.get(&AgentId::from(id)).unwrap().record()
}

#[tokio::test]
async fn roster_is_hydrated_as_children_of_the_loader() {
    let kernel = boot_with_manifest(MANIFEST).await;
    assert!(kernel.agents.contains_key(&AgentId::from("memory")));
    assert!(kernel.agents.contains_key(&AgentId::from("worker")));
    for id in ["memory", "worker"] {
        assert_eq!(
            record(&kernel, id).parent_id.as_deref(),
            Some("static_loader")
        );
    }
}

#[tokio::test]
async fn declared_agents_carry_baseline_plus_explicit_grants() {
    let kernel = boot_with_manifest(MANIFEST).await;
    let can = record(&kernel, "worker").meta["can_call"].clone();
    assert_eq!(can["$self"], json!("*"));
    assert_eq!(can["$parent"], json!("*"));
    assert_eq!(can["$children"], json!("*"));
    assert_eq!(can["memory"], json!(["set", "read"]));
    // memory got the baseline with no explicit can_call in the manifest.
    assert_eq!(
        record(&kernel, "memory").meta["can_call"]["$self"],
        json!("*")
    );
}

#[tokio::test]
async fn roster_is_closed_a_child_cannot_create_more() {
    let kernel = boot_with_manifest(MANIFEST).await;
    let r = with_sender(
        AgentId::from("worker"),
        kernel.send(
            &AgentId::from("core"),
            json!({"type": "create_agent", "handler_module": "mock.tools"}),
        ),
    )
    .await;
    assert_eq!(r["reason"], json!("unauthorized"), "{r}");
}

#[tokio::test]
async fn boot_is_idempotent_by_id() {
    let kernel = boot_with_manifest(MANIFEST).await;
    let before = kernel.agents.len();
    let r = kernel
        .send(&AgentId::from("static_loader"), json!({"type": "boot"}))
        .await;
    assert_eq!(r["booted"], json!(true), "{r}");
    let skipped: Vec<String> = r["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(skipped.contains(&"memory".to_string()) && skipped.contains(&"worker".to_string()));
    assert_eq!(r["created"], json!([]));
    assert_eq!(kernel.agents.len(), before);
}

#[tokio::test]
async fn missing_manifest_reports_not_crashes() {
    let kernel = mk_kernel();
    let rec = kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent", "handler_module": HANDLER_MODULE,
                "id": "sl2", "manifest": "/no/such/path/nope.yaml",
            }),
        )
        .await;
    assert_eq!(rec["id"], json!("sl2"), "{rec}");
    let b = kernel
        .send(&AgentId::from("sl2"), json!({"type": "boot"}))
        .await;
    assert_eq!(b["booted"], json!(false));
    assert!(b["error"]
        .as_str()
        .unwrap()
        .contains("cannot read manifest"));
}
