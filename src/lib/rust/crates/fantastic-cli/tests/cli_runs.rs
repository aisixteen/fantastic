//! Integration tests for the `fantastic` binary.
//!
//! Each test runs the binary in a fresh tempdir so the workdir lock
//! and on-disk state stay scoped.

use std::process::Command;

fn fantastic_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fantastic_kernel")
}

/// Read the FIRST line from a piped child stream, bounded — a plain blocking
/// `read_line` has no deadline, so this hands the read to a throwaway thread
/// and waits on it with a timeout. `None` on timeout or EOF-with-no-line.
///
/// The reader thread keeps DRAINING the stream (discarding everything past
/// the first line) until the writer closes it, rather than dropping the
/// stream the moment it has its answer: dropping early closes the pipe's
/// read end while the child may still be alive, and a later `println!`/
/// `eprintln!` on the now-broken pipe PANICS the child (`std::io::stdio`
/// treats a failed print as fatal) — turning a test assertion into a crash
/// in the very process under test.
fn read_line_bounded<R: std::io::Read + Send + 'static>(
    stream: R,
    timeout: std::time::Duration,
) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
        // Keep draining so the pipe never looks "closed" to the writer.
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
    });
    rx.recv_timeout(timeout)
        .ok()
        .map(|s| s.trim_end().to_string())
}

#[test]
fn no_args_in_virgin_dir_exits_zero_silently() {
    // No web agent persisted → the daemon mode boots, finds nothing
    // worth keeping the process alive, and exits 0.
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(fantastic_bin())
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic");
    assert!(
        output.status.success(),
        "fantastic exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn reflect_returns_uniform_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(fantastic_bin())
        .arg("reflect")
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic reflect");
    assert!(
        output.status.success(),
        "fantastic reflect exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Uniform reflect: id + tree (default all). No primer keys.
    assert!(
        stdout.contains("\"id\""),
        "reflect missing id key: {stdout}"
    );
    assert!(stdout.contains("\"tree\""), "reflect missing tree key");
    // No top-level `transports` PRIMER key (transports docs moved into the readme).
    // Match the key form `"transports":` — the word also appears as a VALUE now
    // (the `readmes` doc index carries a topic keyed "transports"), which is fine.
    assert!(
        !stdout.contains("\"transports\":"),
        "transports should have moved to the readme: {stdout}"
    );
    assert!(
        !stdout.contains("\"available_bundles\""),
        "available_bundles is now the bundles flag: {stdout}"
    );
}

#[test]
fn reflect_bundles_all_lists_catalog() {
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(fantastic_bin())
        .args(["reflect", "bundles=all"])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic reflect bundles=all");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"bundles\"") && stdout.contains("\"handler_module\""),
        "reflect bundles=all missing catalog: {stdout}"
    );
}

/// Wire the persistence provider: a `file_bridge` store rooted at the loader's
/// own `.fantastic`, opened (`ingress_rule=allow_all_ingress`). Persistence is now
/// provider-routed — without a store, records stay in RAM (lost on a one-shot's
/// exit). The store persists ITSELF through itself, so it survives to the next
/// one-shot, after which other agents persist through it.
fn create_store(dir: &std::path::Path) {
    let out = Command::new(fantastic_bin())
        .args([
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=file_bridge.tools",
            r#"allow=["."]"#,
            "id=store",
            "root=.fantastic",
            "ingress_rule=allow_all_ingress",
        ])
        .current_dir(dir)
        .output()
        .expect("create store");
    assert!(
        out.status.success(),
        "create store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn one_shot_create_agent_persists_record() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Wire the store first — it self-persists, so .fantastic/agents/store
    // survives this process and the NEXT one-shot loads it as the provider.
    create_store(tmp.path());
    assert!(
        tmp.path()
            .join(".fantastic/agents/store/agent.yaml")
            .exists(),
        "the store must persist itself through itself"
    );
    let output = Command::new(fantastic_bin())
        .args([
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=file_bridge.tools",
            r#"allow=["."]"#,
            "id=ff",
            "root=/tmp",
            "ingress_rule=allow_all_ingress",
        ])
        .current_dir(tmp.path())
        .output()
        .expect("create_agent");
    assert!(
        output.status.success(),
        "create_agent exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let path = tmp.path().join(".fantastic/agents/ff/agent.yaml");
    assert!(
        path.exists(),
        "agent.yaml not written (through the provider)"
    );
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("file_bridge.tools"));
    assert!(content.contains("/tmp"));
}

/// A14.6, explicitly named per the plan: a one-shot `<id> <verb>` sends
/// `{type:boot}` to the root BEFORE dispatching the call, so a mutation the
/// call makes persists through the record writer that boot subscribes —
/// `one_shot_create_agent_persists_record` is the same proof; this pins the
/// item by its own name.
#[test]
fn one_shot_boots_the_root_before_the_call() {
    let tmp = tempfile::TempDir::new().unwrap();
    create_store(tmp.path());
    let out = Command::new(fantastic_bin())
        .args([
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=file_bridge.tools",
            r#"allow=["."]"#,
            "id=second",
            "root=/tmp",
            "ingress_rule=allow_all_ingress",
        ])
        .current_dir(tmp.path())
        .output()
        .expect("create_agent");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        tmp.path()
            .join(".fantastic/agents/second/agent.yaml")
            .exists(),
        "the root must boot (subscribe the record writer) BEFORE the call \
         dispatches, or this record never reaches disk"
    );
}

#[test]
fn one_shot_dispatch_returns_json_reply() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Wire the provider, then create a file agent that persists through it.
    create_store(tmp.path());
    Command::new(fantastic_bin())
        .args([
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=file_bridge.tools",
            r#"allow=["."]"#,
            "id=ff",
            "root=/tmp",
            "ingress_rule=allow_all_ingress",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    // Now dispatch reflect on it (rehydrated from the persisted record).
    let output = Command::new(fantastic_bin())
        .args(["ff", "reflect"])
        .current_dir(tmp.path())
        .output()
        .expect("reflect on ff");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"id\""));
    assert!(stdout.contains("\"sentence\""));
    assert!(stdout.contains("Filesystem edge of the io family"));
}

/// The vendored `help.md` — the same bytes the binary prints, read from the
/// crate source so a test can compare without shelling out twice.
const HELP_MD: &str = include_str!("../../bundles/fantastic-cli-bundle/src/help.md");

/// A14.1 — `help` / `-h` / `--help` as the FIRST word prints python's
/// `cli/help.md` rstripped plus one `\n`, exits 0, and seeds the workdir like
/// every other mode. Later words are ignored.
#[test]
fn help_prints_cli_help_md_verbatim() {
    let expected = format!("{}\n", HELP_MD.trim_end());
    for spelling in [
        vec!["help"],
        vec!["-h"],
        vec!["--help"],
        // Later words are IGNORED — python checks argv[0] before anything else.
        vec!["help", "foo"],
    ] {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = Command::new(fantastic_bin())
            .args(&spelling)
            .current_dir(tmp.path())
            .output()
            .expect("run fantastic help");
        assert!(
            out.status.success(),
            "{spelling:?} exited non-zero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            expected,
            "{spelling:?} did not print help.md verbatim"
        );
        assert_eq!(out.stdout.len(), 1545, "python's help.md is 1545 bytes");
        assert!(
            tmp.path().join(".fantastic/agent.yaml").exists(),
            "{spelling:?} must seed the workdir like every other mode"
        );
    }
}

/// No readme or `docs/` is ever written into `.fantastic` — not with the cold
/// root record, not through a wired store on a later root persist. The manual
/// is served from the compiled bundle (owner 2026-10-14).
#[test]
fn no_readme_or_docs_land_in_dot_fantastic() {
    let tmp = tempfile::TempDir::new().unwrap();
    let fdir = tmp.path().join(".fantastic");
    let run = |args: &[&str]| {
        let out = Command::new(fantastic_bin())
            .args(args)
            .current_dir(tmp.path())
            .output()
            .expect("run fantastic");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["kernel", "list_agents"]);
    assert!(fdir.join("agent.yaml").exists());
    // A wired store and a root record write — the old warm-seed path.
    run(&[
        "kernel_state",
        "create_agent",
        "parent_id=kernel",
        "handler_module=file_bridge.tools",
        r#"allow=["."]"#,
        "id=store",
        "root=.fantastic",
        "ingress_rule=allow_all_ingress",
    ]);
    run(&[
        "kernel",
        "update_agent",
        "id=kernel_state",
        "description=warm",
    ]);
    assert!(!fdir.join("readme.md").exists(), "no root readme copy");
    assert!(!fdir.join("docs").exists(), "no docs/ copy");
    assert!(
        !fdir.join("agents/store/readme.md").exists(),
        "no per-agent readme copy"
    );
}

/// A14.2 — exactly one remaining word that is not `help`/`reflect`/`init`
/// prints python's four-line usage on stderr and exits 2 (rust used to fall
/// through to daemon mode and exit 0 silently).
#[test]
fn single_positional_prints_usage_and_exits_2() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .arg("wat")
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic wat");
    assert_eq!(out.status.code(), Some(2), "python exits 2");
    assert!(out.stdout.is_empty(), "the usage goes to STDERR");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "usage: fantastic <id> <verb> [k=v ...]\n\
         \u{20}      fantastic reflect [<id>]\n\
         \u{20}      fantastic                              (interactive REPL when stdin is a tty;\n\
         \u{20}                                              daemon when a web agent is persisted)\n",
    );
    assert!(
        tmp.path().join(".fantastic/agent.yaml").exists(),
        "python seeds before it refuses"
    );
}

/// A14.11 — `boot <word>` is a one-shot to an agent called `boot`, not a mode.
#[test]
fn boot_word_is_an_agent_id() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .args(["boot", "reflect"])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic boot reflect");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        // C19: python's `!r` — single quotes, so no JSON escaping at all.
        stdout.contains("no agent 'boot'"),
        "`boot reflect` must dispatch to agent `boot`: {stdout}"
    );
}

/// A14.9 — stdout JSON is python's `json.dumps(reply, indent=2, default=str)`,
/// so it is ensure_ascii: a non-ASCII character leaves as a `\uXXXX` escape.
#[test]
fn one_shot_json_is_python_ensure_ascii() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        // The id is echoed back inside the unknown-agent refusal.
        .args(["caf\u{2014}\u{e9}", "reflect"])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic on a non-ascii id");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("caf\\u2014\\u00e9"),
        "ensure_ascii must escape both code points: {stdout}"
    );
    // Only the JSON REPLY is ensure_ascii-checked — A14.18's trailing
    // kernel-shutdown status line ("no store wired…") is plain human text,
    // exactly as python's own bare `print()` leaves it, raw em dash included.
    let json_only: String = stdout
        .lines()
        .take_while(|l| !l.starts_with("  ["))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !json_only.contains('\u{2014}'),
        "no raw non-ascii may reach the JSON reply: {json_only}"
    );
}

/// A14.17 — stdout carries the reply JSON and NOTHING else. The old
/// `tracing_subscriber` wrote ANSI WARN lines there (a `boot ... errored`
/// warning, a corrupt `agent.yaml` skip), so a caller piping into `jq` broke.
#[test]
fn one_shot_stdout_is_only_the_reply_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    // A record the bundle registry cannot resolve AND a corrupt sibling: both
    // used to print a WARN line through the subscriber.
    std::fs::create_dir_all(tmp.path().join(".fantastic/agents/broken")).unwrap();
    std::fs::write(
        tmp.path().join(".fantastic/agents/broken/agent.yaml"),
        "{ not json",
    )
    .unwrap();
    let out = Command::new(fantastic_bin())
        .args([
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=nope.tools",
            "id=np",
        ])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic create_agent");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The JSON reply OPENS stdout — A14.18's kernel-shutdown status line
    // ("no store wired…", python's own bare `print()`) trails it when (as
    // here) no store was ever wired. Parse only the leading JSON value; the
    // stream deserializer stops there without touching what follows.
    let mut de = serde_json::Deserializer::from_str(&stdout).into_iter::<serde_json::Value>();
    de.next()
        .expect("a json value")
        .unwrap_or_else(|e| panic!("stdout must OPEN with one json document ({e}): {stdout}"));
    assert!(
        !stdout.contains("WARN"),
        "no tracing line may reach stdout: {stdout}"
    );
}

/// A15.3 — no `tracing` formatting reaches stderr either: the subscriber is
/// gone, so nothing prints the `<timestamp>  WARN <target>:` shape python has
/// no equivalent for.
#[test]
fn daemon_stderr_has_no_tracing_prefix() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".fantastic/agents/broken")).unwrap();
    std::fs::write(
        tmp.path().join(".fantastic/agents/broken/agent.yaml"),
        "{ not json",
    )
    .unwrap();
    let out = Command::new(fantastic_bin())
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic (daemon, nothing to serve)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("WARN"), "tracing WARN on stderr: {stderr}");
    assert!(
        !stderr.contains("ERROR"),
        "tracing ERROR on stderr: {stderr}"
    );
    assert!(
        !stderr.contains("agent.yaml"),
        "python skips a corrupt record silently: {stderr}"
    );
}

/// A14.13 — "nothing to serve" (no web agent, no tty) is decided BEFORE the
/// lock and before any boot — python `_default`'s early `return` never calls
/// `FantasticLock()`. Proof: no `lock.yaml` is ever created, and stdout
/// never gets a `[kernel] up` (nothing was ever booted to announce).
#[test]
fn no_web_daemon_boots_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic (daemon, nothing to serve)");
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "no [kernel] up — nothing was ever booted: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !tmp.path().join(".fantastic/lock.yaml").exists(),
        "no lock is ever taken when there is nothing to serve"
    );
}

/// A14.13 — the daemon's lifecycle lines, byte for byte: `[kernel] up` on
/// stdout once booted; a leading blank line + the signal's name + `—
/// shutting down...` on stderr the moment SIGTERM lands; `tearing down
/// agents...` / `down` bracketing the graceful `Kernel::shutdown()` walk.
/// python `kernel/modes/longrun.py:18-129`.
#[test]
fn daemon_output_lines_match_python() {
    let tmp = tempfile::TempDir::new().unwrap();
    // An ephemeral port up front — the web bundle treats `0` as falsy (no
    // port at all), so a real, currently-free port is needed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // A root record too — a VIRGIN dir (no `.fantastic/agent.yaml`) reseeds
    // fresh and hydrates NO children on that boot, python's own rule
    // (`bootstrap.rs`'s `boot_disk`), so a bare `agents/web/agent.yaml` with
    // no root record alongside it is invisible on the very first boot.
    // …and the root names its MEMORY STORE, or it does not start.
    std::fs::create_dir_all(tmp.path().join(".fantastic")).unwrap();
    std::fs::write(
        tmp.path().join(".fantastic/agent.yaml"),
        "{\"id\": \"kernel_state\", \"handler_module\": \"kernel_state.tools\", \
         \"memory_store_id\": \"memory\"}",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join(".fantastic/agents/memory")).unwrap();
    std::fs::write(
        tmp.path().join(".fantastic/agents/memory/agent.yaml"),
        "{\"id\": \"memory\", \"handler_module\": \"ram_store.tools\", \
         \"parent_id\": \"kernel_state\", \"operator\": true}",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join(".fantastic/agents/web")).unwrap();
    std::fs::write(
        tmp.path().join(".fantastic/agents/web/agent.yaml"),
        format!(
            "{{\"id\": \"web\", \"handler_module\": \"web.tools\", \
             \"parent_id\": \"kernel_state\", \"port\": {port}}}"
        ),
    )
    .unwrap();

    let mut child = Command::new(fantastic_bin())
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the daemon");

    let stdout = child.stdout.take().expect("piped stdout");
    let up_line = read_line_bounded(stdout, std::time::Duration::from_secs(10))
        .expect("the daemon must print [kernel] up within 10s");
    assert_eq!(up_line, "[kernel] up");

    // SIGTERM — one of the three signals python installs a handler for.
    // Safety: `child.id()` is a live pid this process owns; signal 0 is
    // never sent (SIGTERM only checks/delivers, it doesn't inspect memory).
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }

    let out = child
        .wait_with_output()
        .expect("the daemon exits after SIGTERM");
    assert!(
        out.status.success(),
        "a clean stop exits 0: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("\n[kernel] SIGTERM — shutting down...\n"),
        "stderr={stderr}"
    );
    assert!(
        stderr.contains("[kernel] tearing down agents...\n"),
        "stderr={stderr}"
    );
    assert!(stderr.contains("[kernel] down\n"), "stderr={stderr}");
    // Order: shutting-down, THEN tearing-down, THEN down.
    let shutting = stderr.find("shutting down...").expect("present");
    let tearing = stderr.find("tearing down agents").expect("present");
    let down = stderr.rfind("[kernel] down").expect("present");
    assert!(shutting < tearing && tearing < down, "stderr={stderr}");
}

/// A14.5 — `.env` is autoloaded as the first step of every mode and the count
/// is announced on stderr. An entry already in the environment always wins.
#[test]
fn dotenv_is_loaded_and_announced() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join(".env"),
        "# a comment\n\nexport FANTASTIC_TEST_A=\"one two\"\nFANTASTIC_TEST_B = 'three'\nFANTASTIC_TEST_C=from_file\nnokey\n",
    )
    .unwrap();
    let out = Command::new(fantastic_bin())
        .arg("reflect")
        .arg("tree=none")
        .current_dir(tmp.path())
        // The shell beats the file: C is already set, so only A and B land.
        .env("FANTASTIC_TEST_C", "from_shell")
        .env_remove("FANTASTIC_TEST_A")
        .env_remove("FANTASTIC_TEST_B")
        .output()
        .expect("run fantastic reflect with a .env");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[kernel] loaded 2 var(s) from .env"),
        "python announces the count on stderr: {stderr}"
    );
    // No .env at all → silence.
    let tmp2 = tempfile::TempDir::new().unwrap();
    let out2 = Command::new(fantastic_bin())
        .arg("reflect")
        .current_dir(tmp2.path())
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&out2.stderr).contains("from .env"),
        "a missing .env is a silent no-op"
    );
}

/// A14.4 — `--ledger-cap <v>` is stripped from argv wherever it sits (and only
/// when a value follows), falls back to `FANTASTIC_LEDGER_CAP`, and a value
/// python's `int()` rejects kills the process with the ValueError text.
#[test]
fn ledger_cap_flag_is_stripped_and_applied() {
    // Stripped mid-argv: the call still resolves to `kernel_state reflect`.
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .args(["--ledger-cap", "5", "kernel_state", "reflect", "tree=none"])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic with --ledger-cap");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("\"id\""),
        "the flag must not become the target id"
    );
    // `inf` is unbounded, not an int parse.
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .args([
            "--ledger-cap",
            "inf",
            "kernel_state",
            "reflect",
            "tree=none",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    // A TRAILING bare flag keeps its place in argv — here it is the lone
    // positional, so python's usage fires.
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .arg("--ledger-cap")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "a bare trailing flag stays argv"
    );
    // The environment fallback, with a value `int()` rejects.
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .arg("reflect")
        .current_dir(tmp.path())
        .env("FANTASTIC_LEDGER_CAP", "abc")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "python dies on int('abc')");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("ValueError: invalid literal for int() with base 10: 'abc'"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A14.7 — a one-shot refuses a LIVE lock with python's `[call] …` line on
/// stderr, prints nothing on stdout and exits 1. `reflect` is unaffected: it
/// never takes the lock. LIVE = the flock is held: this test process takes the
/// daemon's own lock.
#[test]
fn one_shot_refuses_a_live_lock_with_python_text() {
    let tmp = tempfile::TempDir::new().unwrap();
    fantastic_kernel::lock::acquire(tmp.path()).expect("hold the workdir lock");
    let pid = std::process::id();
    let out = Command::new(fantastic_bin())
        .args(["kernel_state", "reflect"])
        .current_dir(tmp.path())
        .output()
        .expect("run a one-shot against a live lock");
    assert_eq!(out.status.code(), Some(1), "python exits 1");
    assert!(out.stdout.is_empty(), "stdout stays empty");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "[call] another fantastic owns this dir (pid={pid}). \
             Stop it, or forward your call over the WS bridge \
             (see ws_bridge bundle).\n"
        ),
    );
    // Read-only `reflect` takes no lock, so it still answers.
    let ok = Command::new(fantastic_bin())
        .args(["reflect", "tree=none"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(ok.status.success(), "reflect is lock-free");
    fantastic_kernel::lock::release(tmp.path()).unwrap();
}

/// Owner 2026-10-03 — a one-shot probes the OS lock, not the pid: a lock file
/// naming a LIVE pid that holds no flock (a dead holder's file, its pid reused)
/// is stale and taken over — `call` and `init` both run, and release it.
#[test]
fn one_shots_take_over_a_lock_whose_pid_holds_no_flock() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".fantastic")).unwrap();
    let mut alive = Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn a live pid");
    let pid = alive.id();
    let stale = || {
        std::fs::write(
            tmp.path().join(".fantastic/lock.yaml"),
            format!("pid: {pid}\n"),
        )
        .unwrap();
    };
    stale();
    let out = Command::new(fantastic_bin())
        .args(["kernel", "list_agents"])
        .current_dir(tmp.path())
        .output()
        .expect("run a one-shot over a stale lock");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"agents\""));
    assert!(
        !tmp.path().join(".fantastic/lock.yaml").exists(),
        "released"
    );
    stale();
    std::fs::write(tmp.path().join("fantastic.yaml"), "agents: []\n").unwrap();
    let out = Command::new(fantastic_bin())
        .args(["init", "static_loader", "./fantastic.yaml"])
        .current_dir(tmp.path())
        .output()
        .expect("run init over a stale lock");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join(".fantastic/lock.yaml").exists(),
        "released"
    );
    let _ = alive.kill();
    let _ = alive.wait();
}

/// C12 + C14: a virgin dir's root IS the `kernel_state` loader, and the daemon
/// boots it FIRST — the boot that rehydrates the ledger, starts the pump and
/// subscribes the record writer. The proof a one-shot can see is that a record
/// created by the SAME invocation is on disk when the process exits: nothing
/// persists without that boot.
#[test]
fn the_root_is_the_kernel_state_loader_and_boots_before_the_roster() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .arg("reflect")
        .current_dir(tmp.path())
        .output()
        .expect("reflect");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"id\": \"kernel_state\""),
        "the root id is python's: {stdout}"
    );
    assert!(
        stdout.contains("\"handler_module\": \"kernel_state.tools\""),
        "the root carries the loader's handler_module: {stdout}"
    );
    // Its frame is the LOADER's contribution, merged on the root only.
    assert!(stdout.contains("\"persistence\""), "{stdout}");
    assert!(stdout.contains("\"guid\""), "{stdout}");

    // The store persists itself in the SAME one-shot that creates it — only the
    // root's boot (and the synchronous drain after the call) makes that true.
    create_store(tmp.path());
    assert!(tmp
        .path()
        .join(".fantastic/agents/store/agent.yaml")
        .exists());
}

/// A14.3 — `init <loader_bundle> <manifest>` creates a loader agent under the
/// root and boots it. python `kernel/modes/dispatch.py:90-149`.
#[test]
fn init_creates_the_loader_under_the_root() {
    let tmp = tempfile::TempDir::new().unwrap();
    // An empty document boots an empty roster (`static_loader`'s own rule) —
    // this test is about `init`'s own routing, not the loader's YAML.
    std::fs::write(tmp.path().join("fantastic.yaml"), "").unwrap();
    let out = Command::new(fantastic_bin())
        .args(["init", "static_loader", "./fantastic.yaml"])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic init");
    assert!(
        out.status.success(),
        "init failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"id\": \"static_loader\""),
        "the loader's id is the bundle name: {stdout}"
    );
    assert!(
        stdout.contains("\"handler_module\": \"static_loader.tools\""),
        "{stdout}"
    );
    assert!(
        stdout.contains("\"manifest\": \"./fantastic.yaml\""),
        "the manifest path rides the record verbatim: {stdout}"
    );
}

/// A14.3 — fewer than 2 args after `init` is python's two-line usage, exit 2.
#[test]
fn init_with_one_arg_prints_usage_exit_2() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .args(["init", "static_loader"])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic init with one arg");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "stdout stays empty");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "usage: fantastic init <loader_bundle> <manifest>\n   \
         e.g. fantastic init static_loader ./fantastic.yaml\n"
    );
    // python builds the kernel BEFORE dispatch, so the usage error still leaves
    // the seeded ROOT RECORD behind, not just the readme + docs tree.
    let rec = tmp.path().join(".fantastic").join("agent.yaml");
    assert!(rec.is_file(), "the usage error still seeds the root record");
    let text = std::fs::read_to_string(&rec).unwrap();
    assert!(
        text.starts_with("id: kernel_state\n") && text.contains("\ncallable_by:"),
        "seeded record: {text}"
    );
}

/// A14.3 — `init` against a LIVE lock prints python's `[init] …` line on
/// stderr, nothing on stdout, and exits 1.
#[test]
fn init_refuses_a_live_lock_with_python_text() {
    let tmp = tempfile::TempDir::new().unwrap();
    fantastic_kernel::lock::acquire(tmp.path()).expect("hold the workdir lock");
    let pid = std::process::id();
    let out = Command::new(fantastic_bin())
        .args(["init", "static_loader", "./fantastic.yaml"])
        .current_dir(tmp.path())
        .output()
        .expect("run init against a live lock");
    assert_eq!(out.status.code(), Some(1), "python exits 1");
    assert!(out.stdout.is_empty(), "stdout stays empty");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("[init] another fantastic owns this dir (pid={pid}). Stop it first.\n"),
    );
    fantastic_kernel::lock::release(tmp.path()).unwrap();
}

/// D7 — a CLI kv argument python coerces to a non-finite float or an int wider
/// than 64 bits must round-trip to the reply, not take the process down.
///
/// `fantastic_kernel kernel create_agent … inf=inf nan=nan big2=<30 digits>`
/// exited 101 with `panicked … invalid number`, and printed nothing where
/// python prints the record (`e2e/parity` `cli/cli/call_create_coerce_*`). Two
/// causes, both from the arbitrary-precision number C74 introduced: `json!` and
/// `serde_json::to_value` re-parse the stored text (`crate::jsonsafe`), and
/// `py_json_dumps` read it through `as_f64().unwrap_or(0.0)`.
#[test]
fn a_non_finite_kv_argument_round_trips_instead_of_panicking() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .args([
            "kernel",
            "create_agent",
            "parent_id=kernel",
            "handler_module=file_bridge.tools",
            "id=coerced",
            "big2=123456789012345678901234567890",
            "inf=inf",
            "nan=nan",
            "neg_inf=-Infinity",
        ])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic kernel create_agent");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "exit {:?}; stderr={stderr}",
        out.status.code()
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    // python `json.dumps(record, indent=2, default=str)` — the bare tokens, and
    // the wide integer's exact digits.
    for want in [
        "\"big2\": 123456789012345678901234567890",
        "\"inf\": Infinity",
        "\"nan\": NaN",
        "\"neg_inf\": -Infinity",
        "\"id\": \"coerced\"",
    ] {
        assert!(stdout.contains(want), "missing {want} in:\n{stdout}");
    }
}

/// Run a one-shot and assert it succeeded — the boilerplate every durability
/// test below repeats.
fn run_cli(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(fantastic_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("run {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Talk to a RUNNING daemon over its REST surface with a raw socket — the CLI
/// carries no HTTP client in its dev-deps, and the wire form is the point.
/// `x-fantastic-execution-id` names the execution, so the ledger row this call
/// settles is identifiable on disk by a string only this test knows.
fn post_rest(port: u16, path: &str, exec_id: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut sock = loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(e) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no listener on {port}: {e}"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };
    sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nconnection: close\r\n\
         content-type: application/json\r\nx-fantastic-execution-id: {exec_id}\r\n\
         content-length: {}\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = sock.read_to_string(&mut out);
    out
}

/// Name the root's MEMORY STORE: a `ram_store` under the root (operator on), and
/// `memory_store_id` on the root. A root without one does not start — the
/// kernel keeps no conversation and no tape; its execution rows live in that
/// store. Needs [`create_store`] first so the records persist.
fn name_memory_store(dir: &std::path::Path) {
    let out = run_cli(
        dir,
        &[
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=ram_store.tools",
            "id=memory",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run_cli(
        dir,
        &[
            "kernel_state",
            "update_agent",
            "id=kernel_state",
            "memory_store_id=memory",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The product path: a one-shot in a tree with a store named. A one-shot is an
/// UNSTARTED kernel: it answers and exits, fast. Python
/// `test_a_one_shot_is_fast_and_clean`.
#[test]
fn a_one_shot_with_a_store_named_is_fast_and_clean() {
    let tmp = tempfile::TempDir::new().unwrap();
    create_store(tmp.path());
    name_memory_store(tmp.path());
    let t0 = std::time::Instant::now();
    let out = run_cli(tmp.path(), &["kernel", "ps"]);
    let wall = t0.elapsed();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wall < std::time::Duration::from_secs(5),
        "one-shot took {wall:?}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"tasks\""), "{stdout}");
    assert!(
        !tmp.path().join(".fantastic/messages.yaml").exists()
            && !tmp.path().join(".fantastic/ledger.json").exists(),
        "the kernel keeps no tape"
    );
}

/// A root with no `memory_store_id` does not start: the daemon fails loud,
/// exits non-zero, names the missing precondition, and releases its lock.
#[test]
fn a_daemon_without_a_memory_store_does_not_start() {
    let tmp = tempfile::TempDir::new().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    create_store(tmp.path());
    let port_kv = format!("port={port}");
    run_cli(
        tmp.path(),
        &[
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=web.tools",
            "id=web",
            &port_kv,
        ],
    );
    let out = Command::new(fantastic_bin())
        .current_dir(tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the daemon");
    assert!(!out.status.success(), "a root with no store must not start");
    assert_eq!(out.status.code(), Some(1), "python's `sys.exit(1)`");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("memory_store_id"), "{stderr}");
    // python `main_dispatch`: `print(f"[kernel] {e}")` for every RuntimeError.
    let line = stderr
        .lines()
        .find(|l| l.contains("memory_store_id"))
        .expect("the fatal line");
    assert!(line.starts_with("[kernel] "), "{line}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("[kernel] up"),
        "it never came up"
    );
    assert!(
        !tmp.path().join(".fantastic/lock.yaml").exists(),
        "the lock must be released"
    );
}

/// A DAEMON with its memory store named stops cleanly at SIGTERM: a live REST
/// call settles, the signal exits 0, and `lock.yaml` is gone — the graceful stop
/// (whose final flush lands the last execution rows in the store) finishes
/// inside the stop window. What lands in the store is pinned in-process by
/// `fantastic-ram-store/tests/kernel_store.rs`: a RAM store is gone with the
/// process, so nothing is on disk to read back here.
#[test]
fn a_daemon_with_a_store_stops_cleanly_and_releases_its_lock() {
    let tmp = tempfile::TempDir::new().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    create_store(tmp.path());
    name_memory_store(tmp.path());
    let port_kv = format!("port={port}");
    run_cli(
        tmp.path(),
        &[
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=web.tools",
            "id=web",
            &port_kv,
            // the host pulls its legs' routes: an explicit edge, both ends
            "can_call={\"$subtree\": [\"get_routes\"]}",
        ],
    );
    // The relay dispatches into the kernel AS ITSELF: `ingress_rule` says who
    // may call IN, `can_call` says where into the kernel it may land, and
    // root's own `callable_by` must name the grandchild.
    run_cli(
        tmp.path(),
        &[
            "kernel_state",
            "create_agent",
            "parent_id=web",
            "handler_module=web_rest.tools",
            "id=rest",
            "ingress_rule=allow_all_ingress",
            "can_call={\"kernel_state\": \"*\"}",
            "callable_by={\"$parent\": [\"get_routes\"]}",
        ],
    );
    run_cli(
        tmp.path(),
        &[
            "kernel_state",
            "update_agent",
            "id=kernel_state",
            "callable_by={\"$self\": \"*\", \"rest\": \"*\"}",
        ],
    );

    let mut child = Command::new(fantastic_bin())
        .current_dir(tmp.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the daemon");
    let stdout = child.stdout.take().expect("piped stdout");
    let up = read_line_bounded(stdout, std::time::Duration::from_secs(10))
        .expect("the daemon must print [kernel] up within 10s");
    assert_eq!(up, "[kernel] up");

    let exec_id = "probe_last_exec";
    let reply = post_rest(
        port,
        "/rest/kernel_state",
        exec_id,
        "{\"type\":\"list_agents\"}",
    );
    assert!(
        reply.contains("\"agents\""),
        "the live REST call must settle: {reply}"
    );

    // Safety: `child.id()` is a live pid this process owns.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let out = child
        .wait_with_output()
        .expect("the daemon exits after SIGTERM");
    assert!(
        out.status.success(),
        "a clean stop exits 0: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join(".fantastic/lock.yaml").exists(),
        "the lock must be released"
    );
}

/// Run one one-shot call in `dir` and parse its stdout — the reply JSON, alone.
fn one_shot_json(dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new(fantastic_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("one-shot");
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not json ({e}): {stdout}"))
}

/// Lib request #3 — python `test_the_catalog_carries_each_backend_kinds_record_schema`:
/// a set-up page reads what a KIND's record takes before any agent of it exists —
/// every field with required/type/enum/default/hint — a key included, as a
/// `credentials` field (a store ref), never an environment variable — from the
/// REAL catalog of this binary.
#[test]
fn the_catalog_carries_each_backend_kinds_record_schema() {
    let tmp = tempfile::TempDir::new().unwrap();
    let r = one_shot_json(
        tmp.path(),
        &["kernel", "reflect", "bundles=all", "tree=none"],
    );
    let by: std::collections::HashMap<&str, &serde_json::Value> = r["bundles"]
        .as_array()
        .expect("bundles")
        .iter()
        .map(|b| (b["name"].as_str().unwrap(), b))
        .collect();
    let vllm = &by["vllm_backend"]["schema"];
    let fields: std::collections::HashMap<&str, &serde_json::Value> = vllm["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["name"].as_str().unwrap(), f))
        .collect();
    let required: Vec<&str> = vllm["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["required"] == serde_json::json!(true))
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(required, ["endpoint", "kv_overflow", "memory_store_id"]);
    // a required field has no default; the counts a record leaves out are read
    // from the server before a turn — no default either
    assert!(required.iter().all(|n| fields[n].get("default").is_none()));
    for n in ["model", "max_tokens", "context_window", "kv_slots"] {
        assert!(fields[n].get("default").is_none(), "{n}");
        assert!(
            fields[n]["hint"]
                .as_str()
                .is_some_and(|h| h.contains("read from the server")),
            "{n}"
        );
    }
    // the defaulted fields carry THE BACKEND'S OWN values
    assert_eq!(fields["temperature"]["default"], serde_json::json!(0.2));
    assert_eq!(
        fields["thinking_token_budget"]["default"],
        serde_json::json!(16000)
    );
    assert_eq!(
        fields["kv_overflow"]["enum"],
        serde_json::json!(["evict", "reject"])
    );
    assert_eq!(
        fields["reasoning_effort"]["enum"],
        serde_json::json!(["low", "medium", "xhigh"])
    );
    // the key: optional where a server may be keyless, required where it never is
    assert_eq!(
        fields["credentials"]["type"],
        serde_json::json!("credentials")
    );
    assert_eq!(fields["credentials"]["required"], serde_json::json!(false));
    let sch = &by["responses_backend"]["schema"];
    assert!(sch.get("key_var").is_none());
    let cred = sch["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "credentials")
        .expect("responses_backend: no credentials field");
    assert_eq!(cred["required"], serde_json::json!(true));
    // the credential store is an installed bundle, and publishes no schema
    assert_eq!(
        by["credential_store"]["handler_module"],
        serde_json::json!("credential_store.tools")
    );
    assert!(by["credential_store"].get("schema").is_none());
    // a bundle that is not a backend publishes none
    assert!(by["file_bridge"].get("schema").is_none());
}

/// The on-device backend is in the REAL catalog of this binary, in every tier,
/// with its record schema: two knobs, the shared AI fields, and `context_window`
/// defaulting to the model's own window (read from the SDK on macOS; Apple's
/// documented 4096 where none can be asked). Reading it runs no prompt.
#[test]
fn the_catalog_carries_the_on_device_backend() {
    let tmp = tempfile::TempDir::new().unwrap();
    let r = one_shot_json(
        tmp.path(),
        &["kernel", "reflect", "bundles=all", "tree=none"],
    );
    let fm = r["bundles"]
        .as_array()
        .expect("bundles")
        .iter()
        .find(|b| b["name"] == serde_json::json!("foundation_models_backend"))
        .unwrap_or_else(|| panic!("no foundation_models_backend in {r}"));
    let fields = fm["schema"]["fields"].as_array().expect("a schema");
    let names: Vec<&str> = fields.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "temperature",
            "max_tokens",
            "memory_store_id",
            "context_window",
            "output_reserve",
            "context_strategy",
            "recent_n",
            "system_prompt",
            "file_bridge_id",
            "system_prompt_file",
            "pre_system_prompt",
            "post_system_prompt"
        ]
    );
    let window = fields[3]["default"].as_i64().expect("an int window");
    assert!(window >= 1, "{window}");
}

/// Lib request #3 — python `test_the_schema_is_what_the_write_enforces`: ONE
/// TRUTH. A create missing the schema's required fields is refused at the write,
/// every one named; an enum value outside its set is refused too.
#[test]
fn the_schema_is_what_the_write_enforces() {
    let tmp = tempfile::TempDir::new().unwrap();
    create_store(tmp.path());
    let create = |extra: &[&str]| {
        let mut args = vec![
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=responses_backend.tools",
        ];
        args.extend_from_slice(extra);
        one_shot_json(tmp.path(), &args)
    };
    let r = create(&["id=a1"]);
    assert_eq!(r["reason"], "invalid_meta", "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("missing: endpoint, model, credentials, memory_store_id"),
        "{r}"
    );
    let r = create(&[
        "id=a2",
        "endpoint=http://127.0.0.1:9/stub",
        "model=m",
        "memory_store_id=memory",
        r#"credentials={"store": "creds", "ref": "claude"}"#,
        "context_strategy=bogus",
    ]);
    assert_eq!(r["reason"], "invalid_meta", "{r}");
    assert!(
        r["error"].as_str().unwrap().contains("context_strategy"),
        "{r}"
    );
    let r = create(&[
        "id=a3",
        "endpoint=http://127.0.0.1:9/stub",
        "model=m",
        "memory_store_id=memory",
        r#"credentials={"store": "creds"}"#,
    ]);
    assert_eq!(r["reason"], "invalid_meta", "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("credentials must be {store"),
        "{r}"
    );
    let r = create(&[
        "id=a4",
        "endpoint=http://127.0.0.1:9/stub",
        "model=m",
        "memory_store_id=memory",
        r#"credentials={"store": "creds", "ref": "claude"}"#,
    ]);
    assert_eq!(r["id"], "a4", "{r}");
}
