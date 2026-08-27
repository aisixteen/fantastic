//! static_loader — declarative YAML roster hydration + the deny-all closure.
//! Mirror of the python `static_loader/tests/test_static_loader.py`.

use super::*;
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{with_sender, Agent, AgentRecord, Item};
use serde_json::{json, Map};

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

/// A manifest file inside a temp dir UNDER the running dir, and the guard that
/// deletes it.
///
/// Inside, because the manifest read obeys the RUNNING-DIR LAW: `fs` is the one
/// disk surface and a cold bring-up read is not exempt, so a manifest under
/// `/tmp` is refused. Python's tests get the same effect by `chdir`-ing into
/// tmp_path (safe there — xdist forks a process per worker); rust test threads
/// share one cwd, so the file comes to the cwd instead of the cwd going to it.
///
/// The TempDir is RETURNED, never `forget`-ten: a leaked fixture is a file that
/// reappears on every run and lands in `git status` forever.
fn write_manifest(text: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::Builder::new()
        .prefix(".sl_fixture_")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let path = dir.path().join("fantastic.yaml");
    std::fs::write(&path, text).unwrap();
    let rel = path
        .strip_prefix(std::env::current_dir().unwrap())
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string();
    (dir, rel)
}

/// Returns the TempDir guard alongside the kernel: the fixture is deleted when
/// the caller drops it, so a test run leaves the tree exactly as it found it.
async fn boot_with_manifest(text: &str) -> (Arc<Kernel>, tempfile::TempDir) {
    let kernel = mk_kernel();
    let (fixture, path) = write_manifest(text);
    // Operator send (top-level → ungated); create_agent auto-boots the loader,
    // which hydrates the roster.
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE,
                "id": "static_loader",
                "manifest": path,
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["id"], json!("static_loader"), "loader create: {rec}");
    (kernel, fixture)
}

fn record(kernel: &Arc<Kernel>, id: &str) -> AgentRecord {
    kernel.agents.get(&AgentId::from(id)).unwrap().record()
}

#[tokio::test]
async fn roster_is_hydrated_as_children_of_the_loader() {
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
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
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
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
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
    let r = with_sender(
        AgentId::from("worker"),
        kernel.send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "mock.tools"}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(r["reason"], json!("unauthorized"), "{r}");
}

#[tokio::test]
async fn boot_is_idempotent_by_id() {
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
    let before = kernel.agents.len();
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
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
async fn an_unreadable_manifest_is_refused_at_write_time() {
    // A broken roster must never become a BOOTING record: `validate_meta`
    // refuses it at create, not one boot later.
    let kernel = mk_kernel();
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
                "id": "sl2", "manifest": "nope.yaml",
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["reason"], "invalid_meta", "{rec}");
    assert!(
        rec["error"].as_str().unwrap().contains("not readable"),
        "{rec}"
    );
    assert!(
        !kernel.agents.contains_key(&AgentId::from("sl2")),
        "a refused create must leave no agent behind"
    );
}

#[tokio::test]
async fn a_manifest_outside_the_running_dir_is_refused() {
    // The cold bring-up read is not exempt from the running-dir law — `fs` is
    // the ONE disk surface, and a roster pointing outside the kernel's own dir
    // would open a file the kernel may not read.
    let kernel = mk_kernel();
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
                "id": "sl3", "manifest": "/etc/hosts",
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["reason"], "invalid_meta", "{rec}");
    assert!(
        rec["error"]
            .as_str()
            .unwrap()
            .contains("escapes the running dir"),
        "{rec}"
    );
}

#[tokio::test]
async fn a_record_without_a_manifest_is_allowed_and_boot_reports_it() {
    // A loader may exist before its roster does; `boot` says so rather than the
    // record being unwritable.
    let kernel = mk_kernel();
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE, "id": "sl4",
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["id"], json!("sl4"), "{rec}");
    let b = kernel
        .send(vec![Item::new(
            AgentId::from("sl4"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(b["booted"], json!(false));
    assert!(
        b["error"].as_str().unwrap().contains("no `manifest` path"),
        "{b}"
    );
}
