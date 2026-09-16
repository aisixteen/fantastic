//! `fantastic_kernel` — the pure **headless host** binary: composes the kernel
//! lib + default bundle set, boots, serves, and blocks. It carries NO
//! interactive front-end (no REPL, no renderer, no PTY) — the `fantastic` CLI
//! product (`src/cli/`) owns all of that and embeds the kernel as a library.
//!
//! Modes, matched in python's order (`P/kernel/modes/dispatch.py:19-41`):
//!
//! 1. `help` / `-h` / `--help` as the FIRST word (later words ignored) —
//!    print the `cli` bundle's `help.md`, exit 0.
//! 2. `fantastic_kernel reflect [<agent_id>] [k=v ...]` — one-shot reflect on
//!    root (or a specific id). No lock acquired; works while a daemon owns
//!    the dir.
//! 3. `fantastic_kernel <agent_id> <verb> [k=v ...]` — one-shot RPC.
//!    Acquires the lock; refuses with python's `[call] …` line if a daemon
//!    already holds it.
//! 4. exactly ONE remaining word — python's 4-line usage on stderr, exit 2.
//! 5. `fantastic_kernel` (no args) — daemon mode. Bootstrap the workdir,
//!    dispatch `boot` on every loaded agent, then block until
//!    SIGINT/SIGTERM (only while a web daemon is serving), then
//!    `shutdown` gracefully. With nothing to serve it boots, reports, exits.
//!
//! Every mode autoloads `.env` first and honours `--ledger-cap` /
//! `FANTASTIC_LEDGER_CAP`, as python's `main_dispatch` does.

use fantastic_bundle::{is_space, py_int, py_json_dumps};
use fantastic_kernel::bootstrap::{self, BootedKernel, BootstrapOptions, DEFAULT_ROOT_ID};
use fantastic_kernel::{lock, AgentId, BundleRegistry, Item};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

fn register_default_bundles() -> BundleRegistry {
    let mut reg = BundleRegistry::new();

    // ── Always-available bundles (work under both `desktop` and `embedded`).
    //
    // None of these spawn subprocesses, fork, or load dynamic libraries,
    // so they all compile + run on iOS. The Bundle trait is the only
    // dependency the kernel cares about; everything below is pure-Rust
    // async over axum/tokio.
    reg.register("file_bridge.tools", fantastic_file::FileBundle);
    // The render sink AI backends stream their turn to. Without it that
    // stream had nowhere to land on rust.
    reg.register(
        fantastic_cli_bundle::HANDLER_MODULE,
        fantastic_cli_bundle::CliBundle,
    );
    // TEST-ONLY. The shared e2e scenarios declare stateful fixture roles
    // (`dispenser`, `flaky_plant`, `purchase_desk`) that python and js register
    // in process. A compiled kernel can only run what is compiled in, and the
    // e2e driver reaches this binary over REST, so without these three the
    // scenarios naming them are SKIPPED. Behind a feature: a default build has
    // none of it.
    #[cfg(feature = "scenario-probes")]
    {
        use fantastic_scenario_probes as probes;
        reg.register(probes::DISPENSER_MODULE, probes::DispenserProbe::default());
        reg.register(probes::PLANT_MODULE, probes::PlantProbe::default());
        reg.register(probes::PURCHASE_MODULE, probes::PurchaseProbe::default());
        // THE HUMAN, as an agent. Every AI probe tasks its model through this
        // one, so the task is a gated `user -> ai` send with a ledger row
        // instead of an operator injection from outside the kernel.
        reg.register(probes::USER_MODULE, probes::UserProbe::default());
        reg.register(probes::ORACLE_MODULE, probes::OracleProbe::default());
    }
    reg.register(
        fantastic_text_file::HANDLER_MODULE,
        fantastic_text_file::TextFileBundle,
    );
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
    // The bridge ships TWO derivations of io_bridge (mirrors py's separate
    // ws_bridge + relay_connector bundles): ws/ssh transports vs the relay-kernel router.
    reg.register("ws_bridge.tools", fantastic_bridge::WsBridgeBundle);
    reg.register(
        "relay_connector.tools",
        fantastic_bridge::RelayConnectorBundle,
    );
    reg.register(
        "nvidia_nim_backend.tools",
        fantastic_nvidia_nim_backend::NvidiaNimBundle,
    );
    reg.register(
        fantastic_llamacpp_backend::HANDLER_MODULE,
        fantastic_llamacpp_backend::LlamaCppBundle,
    );
    reg.register(
        fantastic_anthropic_backend::HANDLER_MODULE,
        fantastic_anthropic_backend::AnthropicBundle,
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
        // Via the exported const: the root reflect's `self_extension`
        // advertises executors by intersecting its table with THIS registry,
        // so a typo'd literal here would silently drop the one executor from
        // what a model is told it has.
        reg.register(
            fantastic_exec_bridge::HANDLER_MODULE,
            fantastic_exec_bridge::ExecBridgeBundle,
        );
    }

    reg
}

#[tokio::main]
async fn main() -> ExitCode {
    // NO tracing subscriber (A14.17). It wrote ANSI WARN lines to STDOUT ahead of
    // a one-shot's reply, so stdout stopped parsing as JSON; python configures no
    // logger at all and prints its diagnostics as plain, explicit lines.

    // A14.5 — `.env` autoload is the FIRST step of every mode (`P/main.py:89-91`).
    let loaded = load_dotenv(Path::new(".env"));
    if loaded > 0 {
        eprintln!("[kernel] loaded {loaded} var(s) from .env");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    // A14.4 — `--ledger-cap` is stripped from argv wherever it sits, then the
    // environment. A value python's `int()` rejects kills the process (python
    // raises the ValueError straight out of `main_dispatch`).
    let (args, cap) = match ledger_cap_from(args) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match dispatch(args, cap).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fantastic_kernel: {e}");
            ExitCode::FAILURE
        }
    }
}

/// python's four-line usage for a single positional with no verb
/// (`P/kernel/modes/dispatch.py:35-40`), byte for byte. Printed to stderr; the
/// process then exits 2.
const USAGE: &str = concat!(
    "usage: fantastic <id> <verb> [k=v ...]\n",
    "       fantastic reflect [<id>]\n",
    "       fantastic                              (interactive REPL when stdin is a tty;\n",
    "                                               daemon when a web agent is persisted)",
);

/// python `_load_dotenv` (`P/kernel/_env.py:9-45`): `KEY=value` lines from `.env`
/// in the cwd into the environment. Blank and `#` lines skipped, a leading
/// `export ` tolerated, matching surrounding quotes stripped, NO expansion — and
/// an entry already in the environment always wins (the shell beats the file).
/// Returns how many vars were actually set.
fn load_dotenv(path: &Path) -> usize {
    if !path.exists() {
        return 0;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let mut count = 0usize;
    for raw in py_splitlines(&text) {
        let mut line = py_strip(raw);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim_start_matches(is_space);
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = py_strip(key);
        let mut val = py_strip(val);
        if key.is_empty() {
            continue;
        }
        let first = val.chars().next();
        if val.chars().count() >= 2
            && matches!(first, Some('\'') | Some('"'))
            && val.chars().next_back() == first
        {
            let mut it = val.chars();
            it.next();
            it.next_back();
            val = it.as_str();
        }
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, val);
            count += 1;
        }
    }
    count
}

/// python `str.splitlines()` — every line boundary CPython recognises, not just
/// `\n`/`\r\n`.
fn py_splitlines(s: &str) -> Vec<&str> {
    const BREAKS: [char; 8] = [
        '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}',
    ];
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let end = if c == '\r' {
            if it.peek().map(|(_, n)| *n) == Some('\n') {
                it.next();
            }
            Some(i)
        } else if c == '\n' || BREAKS.contains(&c) {
            Some(i)
        } else {
            None
        };
        if let Some(end) = end {
            out.push(&s[start..end]);
            start = it.peek().map_or(s.len(), |(j, _)| *j);
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// python `str.strip()` — `str.isspace()`, which covers the four ASCII
/// separators U+001C..U+001F that `str::trim` leaves behind.
fn py_strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// python `str.rstrip()`.
fn py_rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// python `_ledger_cap_from` (`P/main.py:66-85`). `--ledger-cap <v>` is removed
/// from argv wherever it sits — but ONLY when a value follows, so a trailing bare
/// flag stays an ordinary word. With no flag the value comes from
/// `FANTASTIC_LEDGER_CAP`. `inf` means unbounded; anything else must parse as
/// python's `int()`.
///
/// `Ok((argv, None))` = the cap was never set (leave the ledger's own default);
/// `Ok((argv, Some(cap)))` = set it, where `cap` is `None` for unbounded.
#[allow(clippy::type_complexity)]
fn ledger_cap_from(argv: Vec<String>) -> Result<(Vec<String>, Option<Option<usize>>), String> {
    let mut raw: Option<String> = None;
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < argv.len() {
        if argv[i] == "--ledger-cap" && i + 1 < argv.len() {
            raw = Some(argv[i + 1].clone());
            i += 2;
            continue;
        }
        out.push(argv[i].clone());
        i += 1;
    }
    if raw.is_none() {
        raw = std::env::var("FANTASTIC_LEDGER_CAP").ok();
    }
    let Some(raw) = raw else {
        return Ok((out, None));
    };
    if raw == "inf" {
        return Ok((out, Some(None)));
    }
    match py_int(&Value::String(raw)) {
        // A negative ring keeps nothing; `usize` cannot hold it, so it floors at 0.
        Ok(n) => Ok((out, Some(Some(usize::try_from(n).unwrap_or(0))))),
        Err(e) => Err(format!("{}: {e}", e.class_name())),
    }
}

/// Apply the parsed `--ledger-cap` to a freshly bootstrapped kernel. python does
/// this once, on the single kernel `main_dispatch` builds; every rust mode
/// bootstraps its own, so every mode applies it.
fn apply_ledger_cap(booted: &BootedKernel, cap: Option<Option<usize>>) {
    if let Some(cap) = cap {
        booted.kernel.ledger.set_cap(cap);
    }
}

async fn dispatch(
    args: Vec<String>,
    cap: Option<Option<usize>>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let workdir = std::env::current_dir()?;

    // Seed `.fantastic/readme.md` if missing, regardless of which mode
    // we're about to run. Python's bootstrap does this universally —
    // an LLM that walks into a workdir's `.fantastic/` always finds
    // the root readme.md next to `agent.json`/`lock.json`. Idempotent
    // (preserves user-edited content). Best-effort: if the workdir
    // can't be written to, the verb may still work.
    let _ = fantastic_core::seed_root_readme(&workdir);

    // Mode 1: `help` / `-h` / `--help` as the FIRST word — later words are
    // IGNORED (`P/kernel/modes/dispatch.py:19-21`). python has already built the
    // kernel by now, so a virgin dir is seeded before the cheatsheet prints; the
    // bootstrap here takes no lock, so it works under a running daemon.
    if matches!(
        args.first().map(String::as_str),
        Some("-h") | Some("--help") | Some("help")
    ) {
        let opts = BootstrapOptions::one_shot(&workdir);
        let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
        apply_ledger_cap(&booted, cap);
        // python `print(src.read_text(...).rstrip())` — rstripped, one newline.
        println!("{}", py_rstrip(fantastic_cli_bundle::HELP));
        return Ok(ExitCode::SUCCESS);
    }

    // Mode 2: `reflect [<id>] [k=v ...]`. The first token after `reflect`
    // is the target unless it's a `k=v` pair (then target defaults to the
    // root); remaining `k=v` compose the reflect payload — `tree=ids`,
    // `bundles=all`, `readme=true`, etc. (mirrors the Python CLI).
    if args.first().map(String::as_str) == Some("reflect") {
        let opts = BootstrapOptions::one_shot(&workdir);
        let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
        apply_ledger_cap(&booted, cap);
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
            .send(vec![Item::new(
                AgentId::from(target.as_str()),
                Value::Object(payload),
            )])
            .await
            .remove(0)
            .result;
        // A14.9 — python `json.dumps(reply, indent=2, default=str)`: ensure_ascii
        // and python's float repr, not serde's pretty printer.
        println!("{}", py_json_dumps(&reply, true, Some(2)));
        return Ok(ExitCode::SUCCESS);
    }

    // Mode 3: `<id> <verb> [k=v ...]`. `boot` is an agent id like any other
    // (A14.11) — python routes `boot x` here, it never means "daemon".
    if args.len() >= 2 {
        // A14.7 — the lock is probed BEFORE the bootstrap that would take it, so
        // the refusal is python's line rather than the kernel's LockHeld text. A
        // malformed lock.json reads as "nobody holds it", as python's `_read_lock`
        // does.
        if let Some(pid) = lock::current_holder(&workdir).ok().flatten() {
            if lock::pid_alive(pid) {
                eprintln!(
                    "[call] another fantastic owns this dir (pid={pid}). \
                     Stop it, or forward your call over the WS bridge \
                     (see ws_bridge bundle)."
                );
                return Ok(ExitCode::from(1));
            }
        }
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
        apply_ledger_cap(&booted, cap);
        let reply = booted
            .kernel
            .send(vec![Item::new(
                AgentId::from(id.as_str()),
                Value::Object(payload),
            )])
            .await
            .remove(0)
            .result;
        println!("{}", py_json_dumps(&reply, true, Some(2)));
        bootstrap::shutdown(&workdir)?;
        return Ok(ExitCode::SUCCESS);
    }

    // Mode 4: exactly ONE remaining word that is not a subcommand — python's
    // usage on stderr, exit 2 (`P/kernel/modes/dispatch.py:34-41`). The workdir
    // is seeded first, as it is for every other mode.
    if !args.is_empty() {
        let opts = BootstrapOptions::one_shot(&workdir);
        let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
        apply_ledger_cap(&booted, cap);
        eprintln!("{USAGE}");
        return Ok(ExitCode::from(2));
    }

    // Mode 5: daemon.
    let opts = BootstrapOptions::daemon(&workdir);
    let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
    // Seed the root readme if missing.
    let _ = fantastic_core::seed_root_readme(&workdir);
    let kernel = Arc::clone(&booted.kernel);

    // Headless host — the kernel binary carries NO interactive CLI/renderer
    // logic anymore (the `fantastic` PRODUCT owns the TUI/REPL). It just boots,
    // serves, and blocks. `boot` every loaded agent (web binds its listener
    // here); failures are logged + skipped — boot must never abort the daemon.
    for id in booted.loaded.iter() {
        let _ = kernel
            .send(vec![Item::new(id.clone(), json!({"type": "boot"}))])
            .await
            .remove(0)
            .result;
        // A refusal REPLY is silent, as it is on python: `_boot_all_agents`
        // prints only for a RAISED boot, and that line
        // (`  [kernel] boot '<id>' raised: <e>`) lands with A14.13 / C14. The
        // old rust `tracing::warn!` here printed for every refusal, through a
        // subscriber python does not have (A15.3).
    }

    // THE DURABLE LEDGER. After the roster is up (so the store agent exists and
    // `find_store` can see it) and before anything is served: rehydrate settled
    // executions, then install the write-through. Order matters — hydrating
    // after the flusher were installed would rewrite what it just read.
    fantastic_kernel::persistence::install_ledger_persistence(&kernel).await;

    // THE DURABLE MESSAGE LOG, on the same seam and for the same reason. It
    // subscribes at the id HYDRATION saw, not at "now": everything the roster
    // minted while booting is above that cursor and gets replayed to the pump,
    // so the window between reading the log and installing the writer loses
    // nothing. Python has to subscribe before any child exists to get that;
    // here the replay covers it.
    fantastic_kernel::messages_log::install_message_pump(&kernel, booted.messages_after);

    // Composition decision: a headless host only blocks when it's actually
    // serving something (a web daemon). With nothing to serve there's no point
    // holding the process — boot, report, exit. (The interactive front-end that
    // used to keep a bare kernel alive on a tty now lives in the `fantastic`
    // PRODUCT, not here.)
    let has_web = kernel
        .agents
        .iter()
        .any(|e| e.value().handler_module.as_deref() == Some("web.tools"));
    if !has_web {
        bootstrap::shutdown(&workdir)?;
        return Ok(ExitCode::SUCCESS);
    }

    eprintln!(
        "fantastic_kernel: headless host up. {} agent(s) loaded. Ctrl-C to stop.",
        booted.loaded.len()
    );
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = sigterm() => {},
        // A root-only `shutdown_kernel` verb fired (over web_rest / web_ws).
        // The verb already DEFERRED this signal ~100ms (see send.rs) so the
        // ack was enqueued + flushed over the web surface (WS frame across
        // out_tx -> send_task -> sink; HTTP body written by hyper after the
        // handler returned) before we got here. This extra short grace is
        // belt-and-suspenders before we drain the listeners below.
        // Backend-agnostic stop: this exit stops the container (PID 1) or
        // kills the bare process, identically.
        _ = kernel.shutdown_requested() => {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        },
    }
    eprintln!("fantastic_kernel: shutting down...");
    // Issue shutdown on every loaded agent (LIFO so children stop
    // before parents).
    for id in booted.loaded.iter().rev() {
        let _ = kernel
            .send(vec![Item::new(id.clone(), json!({"type": "shutdown"}))])
            .await
            .remove(0)
            .result;
    }
    bootstrap::shutdown(&workdir)?;
    eprintln!("fantastic_kernel: done.");
    Ok(ExitCode::SUCCESS)
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

/// k=v value coercion for one-shot `fantastic <id> <verb> k=v`. Mirrors the
/// canonical python CLI's `_coerce` (kernel/modes/parse.py): the SHARED scalar
/// rule, plus a JSON object/array literal for the OPERATOR.
///
/// The scalar half is [`fantastic_kernel::coerce_scalar`], the same function the
/// model's `<k>v</k>` goes through — ONE rule for text a caller typed, as python
/// has. The JSON branch is this side only: it lets an operator pass a nested
/// payload (`payload={"type":"list_agents"}`, `tags=[1,2]`), and the
/// model↔harness path carries no JSON at all.
fn parse_kv(v: &str) -> Value {
    let typed = fantastic_kernel::coerce_scalar(v);
    if !typed.is_string() {
        return typed;
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

/// Twins of python's `PT/kernel/test_kernel_dotenv.py` and the `--ledger-cap`
/// half of `P/main.py`. Each `.env` case uses its own key names: the loader
/// writes to the PROCESS environment, which the whole test binary shares.
#[cfg(test)]
mod cli_shell_tests {
    use super::{ledger_cap_from, load_dotenv, py_rstrip, py_splitlines, USAGE};

    fn write_env(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(".env");
        std::fs::write(&path, body).unwrap();
        (tmp, path)
    }

    #[test]
    fn a_missing_env_file_is_a_silent_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(load_dotenv(&tmp.path().join(".env")), 0);
    }

    #[test]
    fn basic_pairs_blank_lines_and_comments() {
        let (_t, p) =
            write_env("\n# a comment\n   \nDOTENV_T1_FOO=bar\n# another\nDOTENV_T1_BAZ=qux\n");
        std::env::remove_var("DOTENV_T1_FOO");
        std::env::remove_var("DOTENV_T1_BAZ");
        assert_eq!(load_dotenv(&p), 2);
        assert_eq!(std::env::var("DOTENV_T1_FOO").unwrap(), "bar");
        assert_eq!(std::env::var("DOTENV_T1_BAZ").unwrap(), "qux");
    }

    #[test]
    fn the_shell_wins_over_the_file() {
        let (_t, p) = write_env("DOTENV_T2_FOO=from-file\n");
        std::env::set_var("DOTENV_T2_FOO", "from-shell");
        assert_eq!(load_dotenv(&p), 0, "nothing is actually set");
        assert_eq!(std::env::var("DOTENV_T2_FOO").unwrap(), "from-shell");
    }

    #[test]
    fn matching_surrounding_quotes_are_stripped() {
        let (_t, p) = write_env(
            "DOTENV_T3_A=\"quoted value\"\n\
             DOTENV_T3_B='single quoted'\n\
             DOTENV_T3_C=unquoted\n\
             DOTENV_T3_D=\"mixed'\n",
        );
        for k in ["DOTENV_T3_A", "DOTENV_T3_B", "DOTENV_T3_C", "DOTENV_T3_D"] {
            std::env::remove_var(k);
        }
        assert_eq!(load_dotenv(&p), 4);
        assert_eq!(std::env::var("DOTENV_T3_A").unwrap(), "quoted value");
        assert_eq!(std::env::var("DOTENV_T3_B").unwrap(), "single quoted");
        assert_eq!(std::env::var("DOTENV_T3_C").unwrap(), "unquoted");
        // Mismatched quotes are LEFT as written.
        assert_eq!(std::env::var("DOTENV_T3_D").unwrap(), "\"mixed'");
    }

    #[test]
    fn export_prefix_is_tolerated_including_extra_space() {
        let (_t, p) = write_env("export DOTENV_T4_FOO=bar\nexport  DOTENV_T4_BAZ=qux\n");
        std::env::remove_var("DOTENV_T4_FOO");
        std::env::remove_var("DOTENV_T4_BAZ");
        assert_eq!(load_dotenv(&p), 2);
        assert_eq!(std::env::var("DOTENV_T4_FOO").unwrap(), "bar");
        assert_eq!(std::env::var("DOTENV_T4_BAZ").unwrap(), "qux");
    }

    #[test]
    fn lines_without_an_equals_and_empty_keys_are_skipped() {
        let (_t, p) = write_env("garbage line no equals\n=value\nDOTENV_T5_FOO=bar\n");
        std::env::remove_var("DOTENV_T5_FOO");
        assert_eq!(load_dotenv(&p), 1);
        assert_eq!(std::env::var("DOTENV_T5_FOO").unwrap(), "bar");
    }

    #[test]
    fn everything_after_the_first_equals_is_the_value() {
        let (_t, p) = write_env("DOTENV_T6_URL=postgres://u:p@host/db?sslmode=require\n");
        std::env::remove_var("DOTENV_T6_URL");
        assert_eq!(load_dotenv(&p), 1);
        assert_eq!(
            std::env::var("DOTENV_T6_URL").unwrap(),
            "postgres://u:p@host/db?sslmode=require"
        );
    }

    #[test]
    fn splitlines_takes_every_cpython_line_boundary() {
        // `str::lines` sees ONE line here; python's `splitlines` sees four.
        assert_eq!(
            py_splitlines("a\u{b}b\u{2028}c\u{85}d"),
            vec!["a", "b", "c", "d"]
        );
        assert_eq!(py_splitlines("a\r\nb\rc\n"), vec!["a", "b", "c"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("trailing"), vec!["trailing"]);
    }

    #[test]
    fn rstrip_takes_the_ascii_separators_trim_end_leaves() {
        assert_eq!(py_rstrip("help\n"), "help");
        assert_eq!(py_rstrip("help\u{1f}\u{1c} \n"), "help");
        assert_eq!(py_rstrip("  keep left"), "  keep left");
    }

    #[test]
    fn usage_is_pythons_four_lines() {
        let lines: Vec<&str> = USAGE.split('\n').collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "usage: fantastic <id> <verb> [k=v ...]");
        assert_eq!(lines[1], "       fantastic reflect [<id>]");
        // The alignment IS the text — python pads to these exact columns.
        assert_eq!(lines[2].len(), 84);
        assert_eq!(lines[3].len(), 84);
        assert!(lines[3].starts_with("                                               daemon "));
    }

    #[test]
    fn ledger_cap_is_stripped_only_when_a_value_follows() {
        let argv = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        std::env::remove_var("FANTASTIC_LEDGER_CAP");
        let (rest, cap) = ledger_cap_from(argv(&["--ledger-cap", "5", "a", "b"])).unwrap();
        assert_eq!(rest, argv(&["a", "b"]));
        assert_eq!(cap, Some(Some(5)));
        // Mid-argv, not just leading.
        let (rest, cap) = ledger_cap_from(argv(&["a", "b", "--ledger-cap", "7"])).unwrap();
        assert_eq!(rest, argv(&["a", "b"]));
        assert_eq!(cap, Some(Some(7)));
        // A TRAILING bare flag keeps its place — nothing follows it.
        let (rest, cap) = ledger_cap_from(argv(&["a", "--ledger-cap"])).unwrap();
        assert_eq!(rest, argv(&["a", "--ledger-cap"]));
        assert_eq!(cap, None);
        // `inf` is unbounded.
        assert_eq!(
            ledger_cap_from(argv(&["--ledger-cap", "inf"])).unwrap().1,
            Some(None)
        );
        // python's `int()`, so `_` and a sign parse and a bad literal raises.
        assert_eq!(
            ledger_cap_from(argv(&["--ledger-cap", " +1_000 "]))
                .unwrap()
                .1,
            Some(Some(1000))
        );
        assert_eq!(
            ledger_cap_from(argv(&["--ledger-cap", "abc"])).unwrap_err(),
            "ValueError: invalid literal for int() with base 10: 'abc'"
        );
    }
}
