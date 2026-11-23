//! THE BOOTED TREE IS THE INITIAL TREE (grant model v9, `initial-tree.md`): after the first
//! start of a freshly seeded folder, every agent's id, parent and `can` are exactly the
//! blueprint's. No code writes a line or adds an agent at boot.
//!
//! Its own integration binary: the bundles clamp against the process cwd and the user home
//! is an env var, both process-global (see `boot_kernel_public.rs`).

use std::collections::BTreeMap;

use fantastic_kernel::bootstrap::DEFAULT_ROOT_ID;
use fantastic_tui::{boot_kernel, seed_dir_with, stop_kernel};
use serde_json::{json, Value};

fn folder(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fc-initial-tree-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `id → (parent, can)` as the tree is meant to be.
fn initial_tree() -> BTreeMap<String, (Option<String>, Value)> {
    let star = json!({"create": {"who": ["*"], "what": "*"}});
    let none = Value::Null;
    let rows: Vec<(&str, Option<&str>, Value)> = vec![
        (DEFAULT_ROOT_ID, None, none.clone()),
        ("kernel_memory", Some(DEFAULT_ROOT_ID), none.clone()),
        ("user", Some(DEFAULT_ROOT_ID), json!({"credentials": {"call": ["get"]}})),
        ("store", Some("user"), none.clone()),
        ("prompt_file", Some("store"), json!({"$parent": {"call": ["read", "watch_path", "unwatch_path"]}})),
        ("memory", Some("user"), none.clone()),
        ("credentials", Some("user"), none.clone()),
        ("files", Some("user"), none.clone()),
        ("static_files", Some("files"), json!({"$parent": {"call": ["list", "watch_path", "unwatch_path"]}})),
        ("exec", Some("user"), none.clone()),
        ("home", Some("user"), none.clone()),
        ("term", Some("user"), none.clone()),
        (
            "web",
            Some("user"),
            json!({
                "canvas_root": star.clone(),
                "files": {"call": ["read", "list", "write"]},
                "static_files": {"call": ["read", "list"]},
            }),
        ),
        (
            "canvas_ws",
            Some("web"),
            json!({"canvas_root": star.clone(), "files": {"call": ["read", "list", "write"]}}),
        ),
        ("canvas_page", Some("web"), json!({"static_files": {"call": ["read", "list"]}})),
        ("mcp", Some("web"), json!({"canvas_root": star.clone()})),
        ("mcp_config", Some("mcp"), none.clone()),
        (
            "canvas_root",
            Some("user"),
            json!({
                "canvas_root": {"create": {"who": ["*"], "what": {"ai_chat.tools": {"model": "*"}, "canvas.tools": {}}}},
                "memory": {"call": ["append", "read"]},
                "prompt_file": {"call": ["read"]},
                "credentials": {"call": ["get"]},
            }),
        ),
        ("web_loader", Some("canvas_root"), none.clone()),
        (
            "canvas_garage",
            Some("canvas_root"),
            json!({
                "credentials": {"call": ["get"]},
                "memory": {"call": ["append", "read"]},
                "canvas_garage": {"create": {"who": ["*"], "what": {
                    "ai_chat.tools": {"model": "*"},
                    "exec_bridge.tools": {"allowed": "*"},
                    "file_bridge.tools": {"root": "*"},
                    "anthropic_backend.tools": {"can": {"credentials": {"call": ["get"]}}, "endpoint": "*", "model": "*"},
                }}},
            }),
        ),
        ("canvas_garage_loader", Some("canvas_garage"), none),
    ];
    rows.into_iter().map(|(id, p, can)| (id.to_string(), (p.map(str::to_string), can))).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_booted_tree_equals_the_initial_tree() {
    let home = folder("home");
    std::env::set_var("FANTASTIC_HOME", &home);
    let dir = folder("dir");
    assert_eq!(seed_dir_with(&dir.join(".fantastic"), &[]), Ok(true));
    std::env::set_current_dir(&dir).unwrap();

    let k = boot_kernel(&dir.join(".fantastic"), &dir, &[], None, None).await.expect("boots");
    let booted: BTreeMap<String, (Option<String>, Value)> = k
        .agent_ids()
        .into_iter()
        .filter_map(|id| k.resolve_target(id.as_str()))
        .map(|a| {
            let r = a.record();
            (a.id.0.clone(), (r.parent_id.clone(), r.meta.get("can").cloned().unwrap_or(Value::Null)))
        })
        .collect();
    assert_eq!(booted, initial_tree());
    stop_kernel(&k).await;

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    for p in [&dir, &home] {
        let _ = std::fs::remove_dir_all(p);
    }
}
