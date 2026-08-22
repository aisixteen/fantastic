//! `fantastic-host` — composes the privileged in-proc host kernel and the
//! kernel-manager command sugar. The product owns the runtime (runners +
//! terminal + AI backends), so the host always registers the FULL bundle set
//! into its in-proc kernel and drives everything through the one primitive —
//! `kernel.send(target, payload)`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use fantastic_kernel::bootstrap::{self, BootstrapOptions};
use fantastic_kernel::{AgentId, BundleRegistry, Kernel};
use serde_json::{json, Map, Value};

pub mod gateway;
pub mod secret;
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
        fantastic_static_loader::HANDLER_MODULE,
        fantastic_static_loader::StaticLoaderBundle,
    );
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
    // (The former in-process lua/js script-executor bundles and the standalone
    // author bundle were removed in the de-loop refactor: execution now runs
    // through `exec_bridge` and the authoring loop is emergent — no registration.)
    reg.register(
        "terminal_backend.tools",
        fantastic_terminal_backend::TerminalBackendBundle,
    );
    // (The former per-language executor and the local/ssh runner bundles were
    // removed in the de-loop refactor: execution is now `exec_bridge`, and
    // sub-kernel reach is the ssh connector + exec_bridge — no registration.)
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

/// Ensure the **`cli` sub-kernel** anchor + namespace at `<cwd>/.fantastic/cli` —
/// the CLI as a generic sub-kernel *peer*, folder-anchored (a literal sibling of
/// `<cwd>/.fantastic/web`). The `cli` is the **initiator** sub-kernel: it has disk,
/// so it hydrates its own namespace natively — a direct on-disk write (like
/// [`Workspace::ensure_guid`]'s `identity.json`), NOT through the app-home manager
/// store and NOT a `kernel_state` loader agent (that RPC surface exists only for a
/// sandboxed browser — see `ts/SERVE.md`). Host-independent: this runs whether or
/// not a folder host kernel is spawned.
///
/// Writes two things, mirroring the `web_loader` shape (`kernel_state @ .fantastic/web
/// watch=false alias=web_loader`):
/// - the namespace dir `<cwd>/.fantastic/cli/` (folder-scoped session state lives here);
/// - a **marker anchor record** `<cwd>/.fantastic/agents/cli_loader/agent.json` — the
///   same shape as `web_loader`, tagged `role=initiator kind=subkernel`. It is a
///   descriptor (not executed here); mint-once (an existing anchor is never clobbered).
///
/// **One path, fail loud** (no fallback): an unwritable `.fantastic` errors out
/// naming the path, rather than silently running without the anchor. Returns the
/// namespace path (`<cwd>/.fantastic/cli`) for the vis.
pub fn ensure_cli_subkernel(cwd: &Path) -> std::io::Result<PathBuf> {
    let fantastic = cwd.join(".fantastic");
    let namespace = fantastic.join("cli");
    std::fs::create_dir_all(&namespace)?;
    let anchor_dir = fantastic.join("agents").join("cli_loader");
    std::fs::create_dir_all(&anchor_dir)?;
    let anchor = anchor_dir.join("agent.json");
    // Mint-once · explicit-wins: never clobber an existing anchor (hand-edits / a
    // future guid on it survive).
    if !anchor.exists() {
        let record = json!({
            "id": "cli_loader",
            "handler_module": "kernel_state.tools",
            "alias": "cli_loader",
            "root": ".fantastic/cli",
            "role": "initiator",
            "kind": "subkernel",
            "watch": false,
        });
        std::fs::write(
            &anchor,
            serde_json::to_string_pretty(&record).unwrap_or_default(),
        )?;
    }
    Ok(namespace)
}

/// Persisted product settings file: `<app_home>/settings.json`. Hydrated config
/// (currently the AI backend + model) lives here so the tool works across runs
/// without re-exporting env every launch. An explicit env var always OVERRIDES
/// the file; nothing here is ever guessed.
pub fn settings_path() -> PathBuf {
    app_home().join("settings.json")
}

/// Read the persisted settings (`{}` if absent/unreadable).
pub fn load_settings() -> Value {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

/// Write the settings file (pretty JSON), creating the home dir if needed.
pub fn save_settings(v: &Value) -> std::io::Result<()> {
    let p = settings_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(v).unwrap_or_default())
}

/// Set a dotted key (e.g. `ai.model`) in `root`, auto-creating intermediate maps.
pub fn settings_set(root: &mut Value, key: &str, val: Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let mut cur = root;
    for (i, p) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            if !cur.is_object() {
                *cur = json!({});
            }
            cur[*p] = val.clone();
        } else {
            if !cur.get(*p).map(Value::is_object).unwrap_or(false) {
                cur[*p] = json!({});
            }
            cur = cur.get_mut(*p).unwrap();
        }
    }
}

/// Hydrate the AI env (`FANTASTIC_AI_BACKEND/MODEL/NUM_CTX`) from the persisted
/// settings — ONLY for vars not already set, so an explicit env always wins.
/// Nothing is guessed: an absent setting leaves the var unset → the brain fails
/// loud as designed. Call once at process start, before any backend read.
pub fn hydrate_ai_env() {
    let s = load_settings();
    let ai = s.get("ai");
    let set = |k: &str, v: Option<&str>| {
        if std::env::var_os(k).is_none() {
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                std::env::set_var(k, v);
            }
        }
    };
    let str_at = |k: &str| ai.and_then(|a| a.get(k)).and_then(Value::as_str);
    let backend = str_at("backend");
    set("FANTASTIC_AI_BACKEND", backend);
    set("FANTASTIC_AI_MODEL", str_at("model"));
    // The ollama endpoint override flows settings → env so the brain provisioner's
    // `endpoint_from_env` fallback picks it up (only when the var is unset).
    set("FANTASTIC_OLLAMA_ENDPOINT", str_at("endpoint"));
    if std::env::var_os("FANTASTIC_NUM_CTX").is_none() {
        if let Some(n) = ai.and_then(|a| a.get("num_ctx")).and_then(Value::as_u64) {
            std::env::set_var("FANTASTIC_NUM_CTX", n.to_string());
        }
    }
    // Load the connector's key from the OS keychain into the env (if unset there),
    // so the brain provisioner's `api_key_from_env` picks it up. The key is NEVER
    // read from settings.json — only the keychain.
    if std::env::var_os("FANTASTIC_AI_KEY").is_none() {
        if let Some(b) = backend.filter(|b| !b.is_empty()) {
            if let Some(key) = secret::get_key(b) {
                std::env::set_var("FANTASTIC_AI_KEY", key);
            }
        }
    }
}

/// A read view of the AI connector config — model/backend from settings, and
/// whether a key is present in the keychain (NEVER the raw key).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AiConfig {
    pub backend: Option<String>,
    pub model: Option<String>,
    /// The ollama endpoint override (`ai.endpoint`), if configured. Not secret —
    /// stored in `settings.json` alongside backend/model, never the keychain.
    pub endpoint: Option<String>,
    pub key_present: bool,
}

/// Read the configured AI connector (CRUD: read). Key is reported as a bool only.
pub fn ai_config() -> AiConfig {
    let s = load_settings();
    let ai = s.get("ai");
    let str_at = |k: &str| {
        ai.and_then(|a| a.get(k))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    let backend = str_at("backend");
    let key_present = backend.as_deref().map(secret::has_key).unwrap_or(false);
    AiConfig {
        model: str_at("model"),
        endpoint: str_at("endpoint"),
        backend,
        key_present,
    }
}

/// Set/replace the AI connector (CRUD: create/update): persist backend+model
/// (+ an optional `endpoint`) to settings, and the key (if given) to the OS
/// keychain. The `endpoint` is NOT secret — it lives in `settings.json` next to
/// backend/model; `Some(non-blank)` sets `ai.endpoint`, `None`/blank CLEARS any
/// stale value (so switching backends doesn't leak an old endpoint). Fails loud
/// if a key is given but no keychain is available — nothing half-written.
pub fn set_ai_connector(
    backend: &str,
    model: &str,
    endpoint: Option<&str>,
    key: Option<&str>,
) -> Result<(), String> {
    if let Some(k) = key.filter(|k| !k.trim().is_empty()) {
        secret::set_key(backend, k)?; // keychain first — fail loud before persisting
    }
    let mut s = load_settings();
    settings_set(&mut s, "ai.backend", json!(backend));
    settings_set(&mut s, "ai.model", json!(model));
    match endpoint.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => settings_set(&mut s, "ai.endpoint", json!(e)),
        None => {
            if let Some(ai) = s.get_mut("ai").and_then(Value::as_object_mut) {
                ai.remove("endpoint");
            }
        }
    }
    save_settings(&s).map_err(|e| format!("settings: {e}"))?;
    Ok(())
}

/// Delete the AI connector (CRUD: delete): drop the whole `ai.*` block from
/// settings (backend + model + `ai.endpoint`) + the key from the keychain.
pub fn clear_ai_connector() -> Result<(), String> {
    let s = load_settings();
    if let Some(b) = s
        .get("ai")
        .and_then(|a| a.get("backend"))
        .and_then(Value::as_str)
    {
        secret::delete_key(b)?;
    }
    let mut s = s;
    if let Value::Object(map) = &mut s {
        map.remove("ai");
    }
    save_settings(&s).map_err(|e| format!("settings: {e}"))?;
    Ok(())
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

    #[test]
    fn ensure_cli_subkernel_writes_anchor_and_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let ns = ensure_cli_subkernel(tmp.path()).unwrap();
        // The namespace dir is `<cwd>/.fantastic/cli` (sibling of `.fantastic/web`).
        assert_eq!(ns, tmp.path().join(".fantastic").join("cli"));
        assert!(ns.is_dir());
        // The anchor record mirrors the `web_loader` shape, tagged as a sub-kernel.
        let anchor = tmp
            .path()
            .join(".fantastic")
            .join("agents")
            .join("cli_loader")
            .join("agent.json");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&anchor).unwrap()).unwrap();
        assert_eq!(v["alias"], json!("cli_loader"));
        assert_eq!(v["root"], json!(".fantastic/cli"));
        assert_eq!(v["role"], json!("initiator"));
        assert_eq!(v["kind"], json!("subkernel"));
        assert_eq!(v["handler_module"], json!("kernel_state.tools"));
        assert_eq!(v["watch"], json!(false));
    }

    #[test]
    fn ensure_cli_subkernel_is_mint_once() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_cli_subkernel(tmp.path()).unwrap();
        let anchor = tmp
            .path()
            .join(".fantastic")
            .join("agents")
            .join("cli_loader")
            .join("agent.json");
        // A hand-edit survives a second call (explicit-wins · never regenerated).
        std::fs::write(
            &anchor,
            json!({"id":"cli_loader","hand":"edited"}).to_string(),
        )
        .unwrap();
        ensure_cli_subkernel(tmp.path()).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&anchor).unwrap()).unwrap();
        assert_eq!(v["hand"], json!("edited"));
    }
}
