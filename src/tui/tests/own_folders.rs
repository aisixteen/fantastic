//! `prompt.md` and `.mcp.json` live in their bridges' own folders, and `<state>/readme.md`
//! is rewritten at every boot (issue #563).
//!
//! Its own integration binary: the bundles clamp against the process cwd and the user home
//! is an env var, both process-global; the tests take one lock.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fantastic_kernel::{AgentId, Item, Kernel};
use fantastic_tui::{boot_kernel, seed_dir_with, stop_kernel};
use serde_json::{json, Value};

static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const CHAT: &str = "agents:\n  - id: m\n    handler_module: ram_store.tools\n  - id: chat\n    handler_module: ai_chat.tools\n    memory_store_id: m\n";

fn folder(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fc-own-folders-{name}-{}", std::process::id()));
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A seeded folder with the chat, booted in-process; `home` is the user home for the run.
async fn boot(name: &str) -> (PathBuf, Arc<Kernel>) {
    let home = folder(&format!("{name}-home"));
    std::env::set_var("FANTASTIC_HOME", &home);
    let d = folder(name);
    assert_eq!(seed_dir_with(&d.join(".fantastic"), &[CHAT]), Ok(true));
    std::env::set_current_dir(&d).unwrap();
    let k = boot_kernel(&d.join(".fantastic"), &d, &[], None, None).await.expect("boots");
    (d, k)
}

fn meta(k: &Kernel, id: &str, key: &str) -> Option<Value> {
    k.agents
        .get(&AgentId::from(id))
        .and_then(|a| a.value().record().meta.get(key).cloned())
}

fn bridge_dir(state: &Path, id: &str) -> PathBuf {
    state.join("agents").join("user").join("agents").join(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_prompt_file_lives_in_its_bridges_own_folder() {
    let _g = ONE_AT_A_TIME.lock().await;
    let (d, k) = boot("prompt").await;
    let state = d.join(".fantastic");
    let text = std::fs::read_to_string(bridge_dir(&state, "prompt_file").join("prompt.md"))
        .expect("seeded in the bridge's folder");
    assert!(text.starts_with("{tree}\n\n{self}\n\n{howto}"), "{text}");
    assert!(
        !state.join("prompt.md").exists(),
        "not at the state folder's top"
    );
    assert_eq!(meta(&k, "prompt_file", "root"), Some(Value::from("@self")));
    assert_eq!(
        meta(&k, "prompt_file", "allow"),
        Some(serde_json::json!(["prompt.md"]))
    );
    assert_eq!(
        meta(&k, "chat", "system_prompt_file"),
        Some(Value::from("prompt.md"))
    );
    stop_kernel(&k).await;
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_json_lives_in_the_mcp_config_bridges_folder() {
    let _g = ONE_AT_A_TIME.lock().await;
    let (d, k) = boot("mcp").await;
    let state = d.join(".fantastic");
    let cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(bridge_dir(&state, "mcp_config").join(".mcp.json"))
            .expect("written in the bridge's folder"),
    )
    .unwrap();
    assert!(cfg["mcpServers"]["fantastic"].is_object(), "{cfg}");
    assert_eq!(
        meta(&k, "mcp", "mcp_config_file"),
        Some(Value::from(".mcp.json"))
    );
    assert_eq!(meta(&k, "mcp_config", "root"), Some(Value::from("@self")));
    stop_kernel(&k).await;
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_readme_names_the_live_paths_and_is_rewritten_each_boot() {
    let _g = ONE_AT_A_TIME.lock().await;
    let (d, k) = boot("readme").await;
    let state = d.join(".fantastic");
    let readme = state.join("readme.md");
    let text = std::fs::read_to_string(&readme).expect("written at boot");
    let st = std::fs::canonicalize(&state).unwrap();
    for want in [
        bridge_dir(&st, "prompt_file")
            .join("prompt.md")
            .display()
            .to_string(),
        bridge_dir(&st, "mcp_config")
            .join(".mcp.json")
            .display()
            .to_string(),
        std::fs::canonicalize(&d).unwrap().display().to_string(),
        "send".to_string(),
        "mcp".to_string(),
        "user".to_string(),
        "memory".to_string(),
        "store".to_string(),
    ] {
        assert!(text.contains(&want), "the readme lacks `{want}`:\n{text}");
    }
    stop_kernel(&k).await;
    std::fs::write(&readme, "x").unwrap();
    let k = boot_kernel(&d.join(".fantastic"), &d, &[], None, None).await.expect("boots again");
    assert_eq!(
        std::fs::read_to_string(&readme).unwrap(),
        text,
        "restored from the live values"
    );
    stop_kernel(&k).await;
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

async fn call(k: &Arc<Kernel>, to: &str, payload: Value) -> Value {
    k.send(vec![Item::new(AgentId::from(to), payload)]).await.remove(0).result
}

/// State `<tmp>/s` and work `<tmp>/w`, apart: seeded and booted in-process.
async fn boot_far(name: &str) -> (PathBuf, PathBuf, PathBuf, Arc<Kernel>) {
    let home = folder(&format!("{name}-home"));
    std::env::set_var("FANTASTIC_HOME", &home);
    let t = folder(name);
    let (state, work) = (t.join("s"), t.join("w"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("in_work.txt"), "x").unwrap();
    assert_eq!(seed_dir_with(&state, &[]), Ok(true));
    let k = boot_kernel(&state, &work, &[], None, None).await.expect("boots");
    (t, state, work, k)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_boot_records_the_workdir_and_the_kernel_reports_it() {
    let _g = ONE_AT_A_TIME.lock().await;
    let (t, state, work, k) = boot_far("workdir_recorded").await;
    assert_eq!(k.work_dir().unwrap(), work);
    assert_eq!(k.state_dir().unwrap(), state);
    let root = k.root().unwrap().id.0.clone();
    assert_eq!(meta(&k, &root, "workdir"), Some(Value::from(work.display().to_string())));
    stop_kernel(&k).await;
    let recorded = std::fs::read_to_string(state.join("agent.yaml")).unwrap();
    assert!(recorded.contains("workdir:"), "{recorded}");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_and_files_resolve_against_a_workdir_far_from_the_state() {
    let _g = ONE_AT_A_TIME.lock().await;
    let (t, _state, work, k) = boot_far("workdir_far").await;
    let listed = call(&k, "files", json!({"type": "list", "path": "."})).await;
    let names: Vec<&str> = listed["files"]
        .as_array()
        .map(|f| f.iter().filter_map(|e| e["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(names.contains(&"in_work.txt"), "the files bridge lists the work folder: {listed}");
    assert!(!names.contains(&"agent.yaml"), "not the state folder: {listed}");
    #[cfg(unix)]
    {
        let r = call(&k, "exec", json!({"type": "run", "command": "pwd", "wait_s": 30})).await;
        let want = std::fs::canonicalize(&work).unwrap().display().to_string();
        let ran_there = r["lines"].as_array().is_some_and(|l| l.iter().any(|row| row["line"].as_str() == Some(&want)));
        assert!(ran_there, "exec cwd `.` runs in the work folder: {r}");
    }
    stop_kernel(&k).await;
    let _ = std::fs::remove_dir_all(&t);
}
