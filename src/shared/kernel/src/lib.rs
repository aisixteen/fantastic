//! `fantastic-host` — composes the privileged in-proc host kernel and the
//! kernel-manager command sugar. The product owns the runtime (runners +
//! terminal + AI backends), so the host always registers the FULL bundle set
//! into its in-proc kernel and drives everything through the one primitive —
//! `kernel.send(target, payload)`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use fantastic_kernel::bootstrap::{self, BootstrapOptions};
use fantastic_kernel::{AgentId, BundleRegistry, Kernel};
use serde_json::{json, Map, Value};

pub mod gateway;
pub use gateway::{KernelHandle, Runtime, Workspace};

/// The privileged host bundle set. The product owns the runtime (runners +
/// terminal + AI backends), so it always registers the full set into its host.
pub fn register_host_bundles() -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    reg.register("file_bridge.tools", fantastic_file::FileBundle);
    reg.register("yaml_state.tools", fantastic_yaml_state::YamlStateBundle);
    reg.register("web.tools", fantastic_web::WebBundle);
    reg.register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    reg.register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    reg.register("scheduler.tools", fantastic_scheduler::SchedulerBundle);
    reg.register(
        "ollama_backend.tools",
        fantastic_ollama_backend::OllamaBackendBundle,
    );
    reg.register(
        "nvidia_nim_backend.tools",
        fantastic_nvidia_nim_backend::NvidiaNimBundle,
    );
    reg.register(
        fantastic_anthropic_backend::HANDLER_MODULE,
        fantastic_anthropic_backend::AnthropicBundle,
    );
    reg.register("ws_bridge.tools", fantastic_bridge::WsBridgeBundle);
    reg.register(
        "relay_connector.tools",
        fantastic_bridge::RelayConnectorBundle,
    );
    reg.register(
        fantastic_proxy_agent::HANDLER_MODULE,
        fantastic_proxy_agent::ProxyAgentBundle::new(),
    );
    reg.register(
        fantastic_tools::HANDLER_MODULE,
        fantastic_tools::ToolsBundle::new(),
    );
    reg.register(
        "terminal_backend.tools",
        fantastic_terminal_backend::TerminalBackendBundle,
    );
    reg.register(
        "python_runtime.tools",
        fantastic_python_runtime::PythonRuntimeBundle,
    );
    reg.register(
        "local_runner.tools",
        fantastic_local_runner::LocalRunnerBundle,
    );
    reg.register("ssh_runner.tools", fantastic_ssh_runner::SshRunnerBundle);
    reg
}

/// The app's own state home — `$FANTASTIC_HOME`, else the OS-native per-app data
/// dir (`directories::ProjectDirs::data_dir()`: `~/.local/share/fantastic-tui` on
/// Linux, `~/Library/Application Support/aisixteen.fantastic-tui` on macOS,
/// `%APPDATA%\aisixteen\fantastic-tui\data` on Windows). This is the MANAGER
/// kernel's workdir — its `.fantastic/` store is the app's hydration source. It
/// is NOT the host/workspace dir: those use the cwd ([`Workspace`]). The dir is
/// created if missing; falls back to `~/.fantastic-tui` then the temp dir.
pub fn app_home() -> PathBuf {
    if let Some(h) = std::env::var_os("FANTASTIC_HOME") {
        let p = PathBuf::from(h);
        let _ = std::fs::create_dir_all(&p);
        return p;
    }
    let dir = directories::ProjectDirs::from("", "aisixteen", "fantastic-tui")
        .map(|d| d.data_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fantastic-tui")))
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Compose the privileged MANAGER kernel — disk-backed at [`app_home`], so the
/// app **hydrates from `<app_home>/.fantastic`** and persists across runs (the
/// brain's history, any app agents). Boots every loaded agent. The host /
/// workspace kernels are separate processes rooted at the cwd ([`Workspace`]).
pub async fn compose_manager() -> Result<(Arc<Kernel>, Vec<AgentId>)> {
    let booted = bootstrap::bootstrap(
        register_host_bundles(),
        BootstrapOptions::daemon(app_home()),
    )?;
    boot_loaded(&booted.kernel, &booted.loaded).await;
    Ok((Arc::clone(&booted.kernel), booted.loaded))
}

/// In-memory variant (no disk, no lock, no hydration) — for tests + the headful
/// harness, which must not touch the real app home or contend on its lock.
pub async fn compose_manager_in_memory() -> Result<(Arc<Kernel>, Vec<AgentId>)> {
    let booted = bootstrap::bootstrap(register_host_bundles(), BootstrapOptions::in_memory())?;
    boot_loaded(&booted.kernel, &booted.loaded).await;
    Ok((Arc::clone(&booted.kernel), booted.loaded))
}

async fn boot_loaded(kernel: &Arc<Kernel>, loaded: &[AgentId]) {
    for id in loaded {
        let _ = kernel.send(id, json!({"type":"boot"})).await;
    }
}

/// k=v value coercion (mirrors the kernel CLI): bool → int → float → JSON
/// object/array literal → string.
pub fn parse_kv(v: &str) -> Value {
    match v.to_ascii_lowercase().as_str() {
        "true" => return json!(true),
        "false" => return json!(false),
        _ => {}
    }
    if let Ok(n) = v.parse::<i64>() {
        return json!(n);
    }
    if let Ok(f) = v.parse::<f64>() {
        return json!(f);
    }
    let looks_json =
        (v.starts_with('{') && v.ends_with('}')) || (v.starts_with('[') && v.ends_with(']'));
    if looks_json {
        if let Ok(parsed) = serde_json::from_str::<Value>(v) {
            return parsed;
        }
    }
    Value::String(v.to_string())
}

pub fn add_kvs(payload: &mut Map<String, Value>, kvs: &[&str]) {
    for kv in kvs {
        if let Some((k, v)) = kv.split_once('=') {
            payload.insert(k.to_string(), parse_kv(v));
        }
    }
}

/// Parse a kernel-manager sugar command into `(target, payload)` for `send`.
pub fn parse_command(line: &str) -> Result<(AgentId, Value), String> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let mut p = Map::new();
    match toks.as_slice() {
        [] => Err("empty".into()),
        ["tree"] | ["reflect"] => {
            Ok((AgentId::from("kernel"), json!({"type":"reflect","tree":"ids"})))
        }
        ["reflect", id] => Ok((AgentId::from(*id), json!({"type":"reflect"}))),
        ["create", handler, kvs @ ..] => {
            p.insert("type".into(), json!("create_agent"));
            p.insert("handler_module".into(), json!(*handler));
            add_kvs(&mut p, kvs);
            Ok((AgentId::from("kernel"), Value::Object(p)))
        }
        ["update", id, kvs @ ..] => {
            p.insert("type".into(), json!("update_agent"));
            p.insert("id".into(), json!(*id));
            add_kvs(&mut p, kvs);
            Ok((AgentId::from("kernel"), Value::Object(p)))
        }
        ["delete", id] => Ok((AgentId::from("kernel"), json!({"type":"delete_agent","id":id}))),
        ["send", id, verb, kvs @ ..] => {
            p.insert("type".into(), json!(*verb));
            add_kvs(&mut p, kvs);
            Ok((AgentId::from(*id), Value::Object(p)))
        }
        _ => Err(format!(
            "unknown: {} (try: tree | reflect [id] | create <handler> [k=v] | update <id> k=v | delete <id> | send <id> <verb> [k=v])",
            toks[0]
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kv_coerces_bool_int_float_json_string() {
        assert_eq!(parse_kv("true"), json!(true));
        assert_eq!(parse_kv("False"), json!(false)); // case-insensitive
        assert_eq!(parse_kv("42"), json!(42));
        assert_eq!(parse_kv("1.5"), json!(1.5));
        assert_eq!(
            parse_kv("{\"type\":\"list_agents\"}"),
            json!({"type":"list_agents"})
        );
        assert_eq!(parse_kv("[1,2,3]"), json!([1, 2, 3]));
        assert_eq!(parse_kv("web.tools"), json!("web.tools"));
        // a stray brace stays a string (only well-formed object/array literals parse).
        assert_eq!(parse_kv("a{b"), json!("a{b"));
    }

    fn cmd(line: &str) -> (AgentId, Value) {
        parse_command(line).unwrap()
    }

    #[test]
    fn parse_command_maps_the_sugar() {
        let (t, p) = cmd("tree");
        assert!(t == AgentId::from("kernel"));
        assert_eq!(p, json!({"type":"reflect","tree":"ids"}));

        let (t, p) = cmd("reflect web");
        assert!(t == AgentId::from("web"));
        assert_eq!(p, json!({"type":"reflect"}));

        let (t, p) = cmd("create web.tools port=8080");
        assert!(t == AgentId::from("kernel"));
        assert_eq!(
            p,
            json!({"type":"create_agent","handler_module":"web.tools","port":8080})
        );

        let (t, p) = cmd("update w foo=bar");
        assert!(t == AgentId::from("kernel"));
        assert_eq!(p, json!({"type":"update_agent","id":"w","foo":"bar"}));

        let (t, p) = cmd("delete w");
        assert!(t == AgentId::from("kernel"));
        assert_eq!(p, json!({"type":"delete_agent","id":"w"}));

        let (t, p) = cmd("send web boot");
        assert!(t == AgentId::from("web"));
        assert_eq!(p, json!({"type":"boot"}));
    }

    #[test]
    fn parse_command_rejects_empty_and_unknown() {
        assert!(parse_command("").is_err());
        assert!(parse_command("   ").is_err());
        assert!(parse_command("frobnicate x").is_err());
    }
}
