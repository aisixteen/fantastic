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
use fantastic_kernel::{lock, AgentId, BundleRegistry, Item, KernelError};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

mod sys;

fn register_default_bundles() -> BundleRegistry {
    fantastic_host::default_bundles()
}

#[tokio::main]
async fn main() -> ExitCode {
    // NO tracing subscriber. It wrote ANSI WARN lines to STDOUT ahead of
    // a one-shot's reply, so stdout stopped parsing as JSON; python configures no
    // logger at all and prints its diagnostics as plain, explicit lines.

    // `.env` autoload is the FIRST step of every mode (`P/main.py:89-91`).
    let loaded = load_dotenv(Path::new(".env"));
    if loaded > 0 {
        eprintln!("[kernel] loaded {loaded} var(s) from .env");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--ledger-cap` is stripped from argv wherever it sits, then the
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
            // python `main_dispatch`: `except RuntimeError as e: print(f"[kernel]
            // {e}", file=sys.stderr); sys.exit(1)` — a lock conflict (its
            // 3-line text on `KernelError::LockHeld`'s Display) and a failed
            // `kernel_start` alike.
            eprintln!("[kernel] {e}");
            ExitCode::FAILURE
        }
    }
}

/// A one-shot's bootstrap (`call`, `init`): the disk kernel WITH the workdir lock
/// — the daemon's own flock, taken by the bootstrap. A live holder is `Ok(None)`
/// after `refusal(pid)` (the pid its file names) went to stderr, and the caller
/// exits with status 1. There is no pid pre-check: a lock file naming a pid that
/// is alive but holds no flock (a reused pid) is stale, and taken over. Python
/// `modes/dispatch.py:_lock_or_exit`.
fn one_shot_bootstrap(
    workdir: &Path,
    refusal: impl FnOnce(u32) -> String,
) -> Result<Option<BootedKernel>, Box<dyn std::error::Error>> {
    match bootstrap::bootstrap(
        register_default_bundles(),
        BootstrapOptions::daemon(workdir),
    ) {
        Ok(booted) => Ok(Some(booted)),
        Err(KernelError::LockHeld { pid }) => {
            eprintln!("{}", refusal(pid));
            Ok(None)
        }
        Err(e) => Err(e.into()),
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
        booted.kernel.set_ledger_cap(cap);
    }
}

async fn dispatch(
    args: Vec<String>,
    cap: Option<Option<usize>>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let workdir = std::env::current_dir()?;

    // Mode 1: `help` / `-h` / `--help` as the FIRST word — later words are
    // IGNORED (`P/kernel/modes/dispatch.py:19-21`). The bootstrap here takes no
    // lock, so it writes nothing and works under a running daemon.
    if matches!(
        args.first().map(String::as_str),
        Some("-h") | Some("--help") | Some("help")
    ) {
        let opts = BootstrapOptions::one_shot(&workdir);
        let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
        apply_ledger_cap(&booted, cap);
        // python `print(src.read_text(...).rstrip())` — rstripped, one newline.
        println!("{}", py_rstrip(fantastic_cli_bundle::HELP));
        // Every non-daemon mode ends with an idempotent kernel
        // shutdown (python's atexit → `Kernel.shutdown`), even here where no
        // lock was ever taken.
        booted.kernel.shutdown().await;
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
        // Python `json.dumps(reply, indent=2, default=str)`: ensure_ascii
        // and python's float repr, not serde's pretty printer.
        println!("{}", py_json_dumps(&reply, true, Some(2)));
        // Every non-daemon mode ends with an idempotent kernel shutdown.
        booted.kernel.shutdown().await;
        return Ok(ExitCode::SUCCESS);
    }

    // Mode 2.5: `init <loader_bundle> <manifest>` — seed a
    // declarative loader. python `kernel/modes/dispatch.py:90-149`.
    if args.first().map(String::as_str) == Some("init") {
        let rest = &args[1..];
        if rest.len() < 2 {
            // lockless: the usage error writes nothing
            let opts = BootstrapOptions::one_shot(&workdir);
            let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
            apply_ledger_cap(&booted, cap);
            eprintln!(
                "usage: fantastic init <loader_bundle> <manifest>\n   \
                 e.g. fantastic init static_loader ./fantastic.yaml"
            );
            booted.kernel.shutdown().await;
            return Ok(ExitCode::from(2));
        }
        let loader_name = rest[0].clone();
        let manifest = rest[1].clone();
        // THE DAEMON'S OWN LOCK — the flock the bootstrap takes; a live holder
        // refuses with `init`'s line (see [`one_shot_bootstrap`]).
        let Some(booted) = one_shot_bootstrap(&workdir, |pid| {
            format!("[init] another fantastic owns this dir (pid={pid}). Stop it first.")
        })?
        else {
            return Ok(ExitCode::from(1));
        };
        apply_ledger_cap(&booted, cap);
        let root_id = booted.kernel.root().expect("root").id.clone();
        // Boot the root loader so the new record subscribes + persists.
        let _ = booted
            .kernel
            .send(vec![Item::new(root_id.clone(), json!({"type": "boot"}))])
            .await;
        // `create_agent` boots the loader child → its own boot reads the
        // manifest and hydrates the roster. The kernel never parses YAML;
        // the loader does. No duplicate-init guard: a store-less kernel
        // creates again on every run; only a persisted record makes
        // `create_agent` refuse (the same id twice).
        let reply = booted
            .kernel
            .send(vec![Item::new(
                root_id.clone(),
                json!({
                    "type": "create_agent",
                    // python `"parent_id": kernel.root.id` — the ROOT'S OWN
                    // id, not the reserved `kernel` address.
                    "parent_id": root_id.as_str(),
                    "handler_module": format!("{loader_name}.tools"),
                    "id": loader_name,
                    "manifest": manifest,
                }),
            )])
            .await
            .remove(0)
            .result;
        println!("{}", py_json_dumps(&reply, true, Some(2)));
        booted.kernel.flush_records().await;
        // The lock release, THEN the kernel shutdown — see the same
        // note on Mode 3 below.
        bootstrap::shutdown(&workdir)?;
        booted.kernel.shutdown().await;
        return Ok(ExitCode::SUCCESS);
    }

    // Mode 3: `<id> <verb> [k=v ...]`. `boot` is an agent id like any other
    // — python routes `boot x` here, it never means "daemon".
    if args.len() >= 2 {
        let id = args[0].clone();
        let verb = args[1].clone();
        let mut payload = Map::new();
        payload.insert("type".to_string(), json!(verb));
        for kv in &args[2..] {
            if let Some((k, v)) = kv.split_once('=') {
                payload.insert(k.to_string(), parse_kv(v));
            }
        }
        // THE DAEMON'S OWN LOCK, the flock the bootstrap takes; a live
        // holder refuses with python's `[call]` line, not the kernel's LockHeld
        // text (see [`one_shot_bootstrap`]).
        let Some(booted) = one_shot_bootstrap(&workdir, |pid| {
            format!(
                "[call] another fantastic owns this dir (pid={pid}). Stop it, or forward \
                 your call over the WS bridge (see ws_bridge bundle)."
            )
        })?
        else {
            return Ok(ExitCode::from(1));
        };
        apply_ledger_cap(&booted, cap);
        // BOOT THE ROOT LOADER so this call's mutations (create/update/delete)
        // subscribe and persist. The debounce never ticks before we exit, so
        // the writer is drained synchronously after the call — python's `call`
        // mode does exactly this (`modes/dispatch.py`).
        let _ = booted
            .kernel
            .send(vec![Item::new(
                booted.kernel.root().expect("root").id.clone(),
                json!({"type": "boot"}),
            )])
            .await;
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
        booted.kernel.flush_records().await;
        // The lock release, THEN the kernel shutdown — python's own
        // order: the `with FantasticLock()` block's `__exit__` releases the
        // lock synchronously inside `call()`/`init()`, and `Kernel.shutdown()`
        // runs LAST, from the process's atexit handler.
        bootstrap::shutdown(&workdir)?;
        booted.kernel.shutdown().await;
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
        // Python's `sys.exit(2)` still runs the atexit shutdown chain.
        booted.kernel.shutdown().await;
        return Ok(ExitCode::from(2));
    }

    // Mode 5: daemon. python `_default` (`kernel/modes/longrun.py:18-129`).
    //
    // NO LOCK YET — hydration only, so the "nothing to serve" decision below
    // can be made the way python makes it: BEFORE the lock and before any
    // boot. Python's tty REPL is NOT ported: this binary
    // stays headless on a tty, exactly as its own doc comment declares —
    // the `has_repl` half of python's composition decision has no rust
    // counterpart here, only the web-daemon half.
    let opts = BootstrapOptions::one_shot(&workdir);
    let booted = bootstrap::bootstrap(register_default_bundles(), opts)?;
    apply_ledger_cap(&booted, cap);
    let kernel = Arc::clone(&booted.kernel);

    let has_web = kernel
        .agents
        .iter()
        .any(|e| e.value().handler_module.as_deref() == Some("web.tools"));
    if !has_web {
        // python: `if not web_agents and not has_repl: return` — silent,
        // exit 0, no lock ever taken.
        return Ok(ExitCode::SUCCESS);
    }

    // LOCK-FIRST, before booting — python: "so port binds don't happen on
    // conflict." `?` surfaces `KernelError::LockHeld` through main()'s
    // `[kernel] {e}` printer.
    lock::acquire(&workdir)?;
    // A fresh root's record lands only now, under the lock.
    let mut booted = booted;
    booted.write_seed()?;

    // THE START SEQUENCE — the root's memory store first, then everyone (python
    // `kernel.start(boot_all=True)`). A root with no `memory_store_id` does not
    // start: fail loud, exit non-zero — the lock released and the agents torn
    // down on the way out, as python's `with FantasticLock()` + atexit do.
    let started = kernel.start(true).await;
    if let Some(err) = started
        .get("error")
        .filter(|e| fantastic_bundle::py_truthy(e))
    {
        let mut msg = fantastic_bundle::py_str(err);
        if let Some(hint) = started
            .get("hint")
            .filter(|h| fantastic_bundle::py_truthy(h))
        {
            msg.push_str(&format!("\n  hint: {}", fantastic_bundle::py_str(hint)));
        }
        kernel.shutdown().await;
        bootstrap::shutdown(&workdir)?;
        return Err(msg.into());
    }

    // The stop handlers go in BEFORE `[kernel] up`: whoever reads that line may
    // signal at once, and a SIGTERM that landed before the handler existed took
    // the default action — the process died with no teardown and the lock held
    // (`daemon_output_lines_match_python` flaked on exactly that window).
    let mut stop = match sys::StopSignals::install() {
        Ok(s) => s,
        Err(e) => {
            kernel.shutdown().await;
            bootstrap::shutdown(&workdir)?;
            return Err(format!("cannot install the stop-signal handlers: {e}").into());
        }
    };

    // Python `print("[kernel] up", flush=True)`. The tty half
    // (`cli` intro/booted verbs) has no rust counterpart (above).
    println!("[kernel] up");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let sig = tokio::select! {
        sig = stop.recv() => sig,
        // A root-only `kernel_stop` verb fired (over web_rest / web_ws).
        // The verb already DEFERRED this signal ~100ms (see send.rs) so the
        // ack was enqueued + flushed over the web surface (WS frame across
        // out_tx -> send_task -> sink; HTTP body written by hyper after the
        // handler returned) before we got here. This extra short grace is
        // belt-and-suspenders before we drain the listeners below.
        // Backend-agnostic stop: this exit stops the container (PID 1) or
        // kills the bare process, identically. python's `kernel_stop`
        // verb sets the SAME `stop` event `_request_stop` does, but does NOT
        // go through `_request_stop` — no per-signal line for this branch.
        _ = kernel.shutdown_requested() => {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            "kernel_stop"
        },
    };
    // Python's exact per-OS-signal line (`_request_stop`): a
    // leading blank line, the signal's name, stderr, flushed.
    if sig != "kernel_stop" {
        eprintln!("\n[kernel] {sig} — shutting down...");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
    // ALWAYS-RUN CLEANUP (python's `finally:`): depth-first call each
    // agent's `on_shutdown`/`on_delete` hook. Idempotent — `Kernel::
    // shutdown`'s own completion flag makes a second call a no-op.
    eprintln!("[kernel] tearing down agents...");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    kernel.shutdown().await;
    eprintln!("[kernel] down");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    // Lock release LAST — python's `with FantasticLock():` wraps the whole
    // boot-through-teardown body, so `release_lock()` runs as that block
    // exits, AFTER the teardown prints above.
    bootstrap::shutdown(&workdir)?;
    Ok(ExitCode::SUCCESS)
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
