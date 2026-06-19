//! `fantastic-host` — composes the privileged in-proc host kernel and the
//! kernel-manager command sugar. The product owns the runtime (runners +
//! terminal + AI backends), so the host always registers the FULL bundle set
//! into its in-proc kernel and drives everything through the one primitive —
//! `kernel.send(target, payload)`.

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

/// Compose the privileged host kernel: bootstrap the full bundle set in-memory,
/// then boot every loaded agent. Returns the kernel handle + the loaded agent
/// ids (the product reflects/serves/drives through `kernel.send`).
pub async fn compose_manager() -> Result<(Arc<Kernel>, Vec<AgentId>)> {
    let booted = bootstrap::bootstrap(register_host_bundles(), BootstrapOptions::in_memory())?;
    let kernel = Arc::clone(&booted.kernel);
    for id in &booted.loaded {
        let _ = kernel.send(id, json!({"type":"boot"})).await;
    }
    Ok((kernel, booted.loaded))
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
