//! `fantastic` CLI — composes the kernel + default bundle set.
//!
//! Modes (matched in order):
//!
//! 1. `fantastic` (no args) — daemon mode. Bootstrap the workdir,
//!    dispatch `boot` on every loaded agent, then block until
//!    SIGINT/SIGTERM, then `shutdown` gracefully.
//! 2. `fantastic reflect [<agent_id>]` — one-shot reflect on root
//!    (or specific id). No lock acquired; works while a daemon owns
//!    the dir.
//! 3. `fantastic <agent_id> <verb> [k=v ...]` — one-shot RPC.
//!    Acquires the lock; refuses with a clear message if a daemon
//!    already holds it.

use fantastic_kernel::bootstrap::{self, BootstrapOptions, DEFAULT_ROOT_ID};
use fantastic_kernel::{AgentId, BundleRegistry};
use serde_json::{json, Map, Value};
use std::sync::Arc;

fn register_default_bundles() -> BundleRegistry {
    let mut reg = BundleRegistry::new();

    // ── Always-available bundles (work under both `desktop` and `embedded`).
    //
    // None of these spawn subprocesses, fork, or load dynamic libraries,
    // so they all compile + run on iOS. The Bundle trait is the only
    // dependency the kernel cares about; everything below is pure-Rust
    // async over axum/tokio.
    reg.register("file.tools", fantastic_file::FileBundle);
    reg.register("yaml_state.tools", fantastic_yaml_state::YamlStateBundle);
    reg.register("web.tools", fantastic_web::WebBundle);
    reg.register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    reg.register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    reg.register("html_agent.tools", fantastic_html_agent::HtmlAgentBundle);
    reg.register(
        "canvas_backend.tools",
        fantastic_canvas_backend::CanvasBackendBundle,
    );
    reg.register(
        "canvas_webapp.tools",
        fantastic_canvas_webapp::CanvasWebappBundle,
    );
    reg.register("scheduler.tools", fantastic_scheduler::SchedulerBundle);
    reg.register("gl_agent.tools", fantastic_gl_agent::GlAgentBundle);
    reg.register(
        "telemetry_pane.tools",
        fantastic_telemetry_pane::TelemetryPaneBundle,
    );
    reg.register(
        "ai_chat_webapp.tools",
        fantastic_ai_chat_webapp::AiChatWebappBundle,
    );
    reg.register(
        "terminal_webapp.tools",
        fantastic_terminal_webapp::TerminalWebappBundle,
    );
    reg.register(
        "ollama_backend.tools",
        fantastic_ollama_backend::OllamaBackendBundle,
    );
    reg.register(
        "kernel_bridge.tools",
        fantastic_kernel_bridge::KernelBridgeBundle,
    );
    reg.register(
        "nvidia_nim_backend.tools",
        fantastic_nvidia_nim_backend::NvidiaNimBundle,
    );
    reg.register(
        fantastic_proxy_agent::HANDLER_MODULE,
        fantastic_proxy_agent::ProxyAgentBundle::new(),
    );
    reg.register(
        fantastic_tools::HANDLER_MODULE,
        fantastic_tools::ToolsBundle::new(),
    );

    // ── Full-tier-only bundles (subprocess / dynamic-loading / etc.).
    //
    // These spawn child processes (`std::process::Command` /
    // `tokio::process::Command`). The `embedded` build (iOS Lite,
    // visionOS, sandboxed macOS) compiles cleanly without them — the
    // `optional = true` dep + `#[cfg(feature = "full")]` gate enforces
    // that at compile time.
    #[cfg(feature = "full")]
    {
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
    }

    reg
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,fantastic=info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(args).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fantastic: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn dispatch(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let workdir = std::env::current_dir()?;

    // Seed `.fantastic/readme.md` if missing, regardless of which mode
    // we're about to run. Python's bootstrap does this universally —
    // an LLM that walks into a workdir's `.fantastic/` always finds
    // the root readme.md next to `agent.json`/`lock.json`. Idempotent
    // (preserves user-edited content). Best-effort: if the workdir
    // can't be written to, the verb may still work.
    let _ = fantastic_core::seed_root_readme(&workdir);

    // Mode 2: `reflect [<id>] [k=v ...]`. The first token after `reflect`
    // is the target unless it's a `k=v` pair (then target defaults to the
    // root); remaining `k=v` compose the reflect payload — `tree=ids`,
    // `bundles=all`, `readme=true`, etc. (mirrors the Python CLI).
    if args.first().map(String::as_str) == Some("reflect") {
        let opts = BootstrapOptions::one_shot(&workdir);
        let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
        let rest = &args[1..];
        let (target, kvs): (String, &[String]) = match rest.first() {
            Some(first) if !first.contains('=') => (first.clone(), &rest[1..]),
            _ => (DEFAULT_ROOT_ID.to_string(), rest),
        };
        let mut payload = Map::new();
        payload.insert("type".to_string(), json!("reflect"));
        for kv in kvs {
            if let Some((k, v)) = kv.split_once('=') {
                payload.insert(k.to_string(), parse_kv(v));
            }
        }
        let reply = booted
            .kernel
            .send(&AgentId::from(target.as_str()), Value::Object(payload))
            .await;
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }

    // Mode 3: `<id> <verb> [k=v ...]`
    if args.len() >= 2 && args[0] != "boot" {
        let id = args[0].clone();
        let verb = args[1].clone();
        let mut payload = Map::new();
        payload.insert("type".to_string(), json!(verb));
        for kv in &args[2..] {
            if let Some((k, v)) = kv.split_once('=') {
                payload.insert(k.to_string(), parse_kv(v));
            }
        }
        let opts = BootstrapOptions::daemon(&workdir);
        let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
        let reply = booted
            .kernel
            .send(&AgentId::from(id.as_str()), Value::Object(payload))
            .await;
        println!("{}", serde_json::to_string_pretty(&reply)?);
        bootstrap::shutdown(&workdir)?;
        return Ok(());
    }

    // Mode 1: daemon.
    let opts = BootstrapOptions::daemon(&workdir);
    let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
    // Seed the root readme if missing.
    let _ = fantastic_core::seed_root_readme(&workdir);
    let kernel = Arc::clone(&booted.kernel);

    // Attach the stdout renderer BEFORE the boot loop so its state
    // subscriber observes the boot events (created/send/emit). Without
    // an early attach, the renderer comes online after boot completes
    // and a quiet daemon shows nothing until the first external event.
    // Python parity — `core.run()` composes the cli renderer before
    // running its boot phase when stdin.isatty().
    use std::io::IsTerminal;
    let has_tty = std::io::stdin().is_terminal();
    let _cli_token = if has_tty {
        Some(fantastic_cli_bundle::attach(&kernel))
    } else {
        None
    };

    // `boot` every loaded agent. The web bundle uses this to spawn its
    // axum listener; other bundles use it for whatever lazy init they
    // need. Failures are logged + skipped — boot must never abort the
    // daemon (matches Python's behaviour).
    for id in booted.loaded.iter() {
        let reply = kernel.send(id, json!({"type": "boot"})).await;
        if let Some(err) = reply.get("error").and_then(Value::as_str) {
            tracing::warn!(agent = %id, error = %err, "boot failed");
        }
    }

    // Composition decision: block on a web daemon OR on an attached
    // tty. Else exit silently.
    let has_web = kernel
        .agents
        .iter()
        .any(|e| e.value().handler_module.as_deref() == Some("web.tools"));
    if !has_web && !has_tty {
        bootstrap::shutdown(&workdir)?;
        return Ok(());
    }

    // Wait for SIGINT/SIGTERM, then graceful shutdown.
    eprintln!(
        "fantastic: daemon up. {} agent(s) loaded. Ctrl-C to stop.",
        booted.loaded.len()
    );
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = sigterm() => {},
    }
    eprintln!("fantastic: shutting down...");
    // Issue shutdown on every loaded agent (LIFO so children stop
    // before parents).
    for id in booted.loaded.iter().rev() {
        let _ = kernel.send(id, json!({"type": "shutdown"})).await;
    }
    bootstrap::shutdown(&workdir)?;
    eprintln!("fantastic: done.");
    Ok(())
}

#[cfg(unix)]
async fn sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut s = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending::<()>().await,
    };
    s.recv().await;
}

#[cfg(not(unix))]
async fn sigterm() {
    std::future::pending::<()>().await
}

/// k=v value coercion for one-shot `fantastic <id> <verb> k=v`. Mirrors
/// the canonical Python CLI's `_coerce` (kernel/_modes.py): case-
/// insensitive bool → int → float → JSON object/array literal → string.
/// The JSON case lets callers pass nested payloads, e.g.
/// `payload={"type":"list_agents"}` or `tags=[1,2]`.
fn parse_kv(v: &str) -> Value {
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
    // Only attempt JSON when it *looks* like an object/array, so a plain
    // string with a stray brace doesn't get misparsed (matches Python).
    let looks_json =
        (v.starts_with('{') && v.ends_with('}')) || (v.starts_with('[') && v.ends_with(']'));
    if looks_json {
        if let Ok(parsed) = serde_json::from_str::<Value>(v) {
            return parsed;
        }
    }
    Value::String(v.to_string())
}

#[cfg(test)]
mod parse_kv_tests {
    use super::parse_kv;
    use serde_json::json;

    #[test]
    fn coerces_like_python() {
        // bool (case-insensitive, matching Python's `v.lower()`).
        assert_eq!(parse_kv("true"), json!(true));
        assert_eq!(parse_kv("True"), json!(true));
        assert_eq!(parse_kv("FALSE"), json!(false));
        // int then float.
        assert_eq!(parse_kv("42"), json!(42));
        assert_eq!(parse_kv("1.5"), json!(1.5));
        // JSON object/array literal — regression: previously stayed a
        // string, so `payload={...}` reached handlers as text and the
        // scheduler errored "payload.type required".
        assert_eq!(
            parse_kv("{\"type\":\"list_agents\"}"),
            json!({"type": "list_agents"})
        );
        assert_eq!(parse_kv("[1,2,3]"), json!([1, 2, 3]));
        // plain strings (incl. a stray brace) stay strings.
        assert_eq!(parse_kv("hello"), json!("hello"));
        assert_eq!(parse_kv("a{b"), json!("a{b"));
        assert_eq!(parse_kv("web.tools"), json!("web.tools"));
    }
}
