//! `prompt.md` and `.mcp.json` live in their bridges' own folders, and `<state>/readme.md`
//! is rewritten at every boot (issue #563).
//!
//! Its own integration binary: the bundles clamp against the process cwd and the user home
//! is an env var, both process-global; the tests take one lock.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fantastic_kernel::{AgentId, Kernel};
use fantastic_tui::{boot_kernel, seed_dir_with, stop_kernel};
use serde_json::Value;

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
    assert_eq!(seed_dir_with(&d, &[CHAT]), Ok(true));
    std::env::set_current_dir(&d).unwrap();
    let k = boot_kernel(&d, &[], None, None).await.expect("boots");
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
    let k = boot_kernel(&d, &[], None, None).await.expect("boots again");
    assert_eq!(
        std::fs::read_to_string(&readme).unwrap(),
        text,
        "restored from the live values"
    );
    stop_kernel(&k).await;
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}
