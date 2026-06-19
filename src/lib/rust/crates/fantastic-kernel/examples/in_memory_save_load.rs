//! End-to-end selftest for the in-memory kernel's save/load surface.
//!
//! Walks through the brain-kernel use case step by step, printing what
//! each call does + the actual JSON snapshot. No filesystem touched at
//! any point.
//!
//! Run: `cargo run -p fantastic-kernel --example in_memory_save_load`

use async_trait::async_trait;
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::{AgentId, Kernel, StorageMode};
use serde_json::{json, Value};
use std::sync::Arc;

/// Trivial bundle the demo's agents use as their handler_module.
struct Demo;

#[async_trait]
impl Bundle for Demo {
    fn name(&self) -> &str {
        "demo"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(Value::Null))
    }
}

fn registry() -> BundleRegistry {
    let mut r = BundleRegistry::new();
    r.register("demo.tools", Demo);
    r
}

fn banner(s: &str) {
    println!("\n────────────────────────────────────────────────────────────");
    println!(" {s}");
    println!("────────────────────────────────────────────────────────────");
}

fn assert_or_die(cond: bool, label: &str) {
    if cond {
        println!("  ✓ {label}");
    } else {
        eprintln!("  ✗ {label}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn main() {
    banner("Step 1: boot an in-memory kernel");
    // Use a fresh tempdir as cwd so the "no fs leakage" assertions
    // aren't fooled by leftover dirs from earlier test runs in the
    // real working dir. We don't pull in `tempfile` (a dev-only dep)
    // since `examples/` only sees main deps — just synthesize a
    // pid-suffixed dir under the OS temp.
    let cwd = std::env::temp_dir().join(format!("fk_in_memory_demo_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).expect("mkdir tempdir");
    let prev_cwd = std::env::current_dir().ok();
    std::env::set_current_dir(&cwd).expect("chdir tempdir");
    let booted = bootstrap(registry(), BootstrapOptions::in_memory()).expect("boot");
    let kernel = Arc::clone(&booted.kernel);
    println!("  bootstrap mode: {:?}", kernel.storage);
    println!("  cwd (sandboxed tempdir): {}", cwd.display());
    assert_or_die(
        kernel.storage == StorageMode::InMemory,
        "kernel.storage == InMemory",
    );
    assert_or_die(
        !cwd.join(".fantastic").exists(),
        "no .fantastic/ dir in cwd",
    );
    assert_or_die(kernel.root().is_some(), "root agent registered");

    banner("Step 2: create three agents (alpha, beta, gamma)");
    for id in ["alpha", "beta", "gamma"] {
        let r = kernel
            .send(
                &AgentId::from("core"),
                json!({"type":"create_agent","handler_module":"demo.tools","id":id,"display_name":format!("Agent {id}")}),
            )
            .await;
        println!("  create_agent {id} → ok={}", r["ok"]);
    }
    assert_or_die(
        kernel.agents.contains_key(&AgentId::from("alpha")),
        "alpha in kernel.agents",
    );
    assert_or_die(
        !cwd.join(".fantastic").exists(),
        "STILL no .fantastic/ dir on disk after 3 creates",
    );

    banner("Step 3: kernel.save_json() — capture the in-RAM snapshot");
    let snapshot = kernel.save_json();
    println!("  snapshot length: {} bytes", snapshot.len());
    println!("  snapshot:");
    // Pretty-print for the human.
    let pretty: Value = serde_json::from_str(&snapshot).unwrap();
    for line in serde_json::to_string_pretty(&pretty).unwrap().lines() {
        println!("    {line}");
    }
    assert_or_die(snapshot.contains("alpha"), "snapshot mentions alpha");
    assert_or_die(snapshot.contains("\"version\":1"), "version field present");

    banner("Step 4: spin a FRESH in-memory kernel + kernel.load_json(snapshot)");
    let booted2 = bootstrap(registry(), BootstrapOptions::in_memory()).expect("boot 2");
    let kernel2 = Arc::clone(&booted2.kernel);
    println!(
        "  fresh kernel agents before load: {:?}",
        agent_ids(&kernel2)
    );
    kernel2.load_json(&snapshot).expect("load");
    println!(
        "  fresh kernel agents after load:  {:?}",
        agent_ids(&kernel2)
    );
    for id in ["core", "alpha", "beta", "gamma"] {
        assert_or_die(
            kernel2.agents.contains_key(&AgentId::from(id)),
            &format!("{id} present in restored kernel"),
        );
    }

    banner("Step 5: save() the restored kernel — must equal the original snapshot");
    let snapshot2 = kernel2.save_json();
    assert_or_die(snapshot == snapshot2, "round-trip byte-identical");

    banner("Step 6: mutate the restored kernel + show the snapshot diff");
    kernel2
        .send(
            &AgentId::from("core"),
            json!({"type":"update_agent","id":"alpha","note":"tutor pipeline"}),
        )
        .await;
    kernel2
        .send(
            &AgentId::from("core"),
            json!({"type":"delete_agent","id":"gamma"}),
        )
        .await;
    let snapshot3 = kernel2.save_json();
    let pretty: Value = serde_json::from_str(&snapshot3).unwrap();
    println!("  post-mutation snapshot:");
    for line in serde_json::to_string_pretty(&pretty).unwrap().lines() {
        println!("    {line}");
    }
    assert_or_die(
        !kernel2.agents.contains_key(&AgentId::from("gamma")),
        "gamma deleted",
    );
    assert_or_die(snapshot3.contains("tutor pipeline"), "note field on alpha");
    assert_or_die(
        !cwd.join(".fantastic").exists(),
        "STILL no .fantastic/ dir on disk (entire in-memory round-trip)",
    );

    banner("Step 7: load_json validation — bad snapshots fail loud");
    let bad = json!({"version": 9999, "agents": []}).to_string();
    let err = kernel2.load_json(&bad).unwrap_err();
    println!("  future-version snapshot → {err}");
    assert_or_die(
        format!("{err}").contains("exceeds"),
        "future version rejected",
    );

    let bad2 = json!({
        "version": 1,
        "agents": [
            {"id": "x", "parent_id": "no-such-parent", "handler_module": "demo.tools"}
        ]
    })
    .to_string();
    let err2 = kernel2.load_json(&bad2).unwrap_err();
    println!("  missing-root snapshot → {err2}");
    assert_or_die(
        format!("{err2}").contains("no root"),
        "missing root rejected",
    );

    banner("ALL 15 ASSERTIONS GREEN — in-memory save/load surface healthy");
    if let Some(p) = prev_cwd {
        let _ = std::env::set_current_dir(p);
    }
    let _ = std::fs::remove_dir_all(&cwd);
}

fn agent_ids(kernel: &Kernel) -> Vec<String> {
    let mut out: Vec<String> = kernel.agents.iter().map(|e| e.key().0.clone()).collect();
    out.sort();
    out
}
