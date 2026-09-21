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

/// A14.18 — every non-daemon mode ends with an idempotent `Kernel::shutdown()`
/// (python's atexit → `Kernel.shutdown`). Observable here via the message
/// pump's own line, which only prints from inside `flush_pending` at
/// shutdown when the pump has pending traffic and no store is wired — rust
/// `bootstrap::shutdown` alone (lock release) never touched this.
#[test]
fn one_shot_runs_kernel_shutdown_at_exit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .args(["kernel_state", "reflect"])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic kernel_state reflect");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("  [messages] no store wired — messages stay in RAM"),
        "kernel shutdown must have run and flushed the message pump: {stdout}"
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
            .join(".fantastic/agents/store/agent.json")
            .exists(),
        "the store must persist itself through itself"
    );
    let output = Command::new(fantastic_bin())
        .args([
            "kernel_state",
            "create_agent",
            "parent_id=kernel",
            "handler_module=file_bridge.tools",
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
    let path = tmp.path().join(".fantastic/agents/ff/agent.json");
    assert!(
        path.exists(),
        "agent.json not written (through the provider)"
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
            .join(".fantastic/agents/second/agent.json")
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
            tmp.path().join(".fantastic/agent.json").exists(),
            "{spelling:?} must seed the workdir like every other mode"
        );
    }
}

/// D6 — the root readme + `docs/` tree are seeded ONLY when the root record is
/// seeded. python's cold seed rides `write_record`, which `main._bootstrap`
/// calls under `if seeded:`; a workdir that already holds a parseable
/// `.fantastic/agent.json` keeps whatever sidecars it has (measured against
/// python 2026-09-20: it writes neither `readme.md` nor `docs/`).
#[test]
fn the_root_readme_and_docs_are_seeded_only_with_the_root_record() {
    // Virgin dir: both land, next to the record python writes.
    let virgin = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .args(["kernel", "list_agents"])
        .current_dir(virgin.path())
        .output()
        .expect("run fantastic in a virgin dir");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(virgin.path().join(".fantastic/agent.json").exists());
    assert!(virgin.path().join(".fantastic/readme.md").exists());
    assert!(virgin
        .path()
        .join(".fantastic/docs/persistence.md")
        .exists());

    // A record already on disk: python seeds nothing, so neither does rust.
    let seeded = tempfile::TempDir::new().unwrap();
    let fdir = seeded.path().join(".fantastic");
    std::fs::create_dir_all(&fdir).unwrap();
    std::fs::write(
        fdir.join("agent.json"),
        r#"{"id": "kernel_state", "handler_module": "kernel_state.tools", "guid": "g"}"#,
    )
    .unwrap();
    let out = Command::new(fantastic_bin())
        .args(["kernel", "list_agents"])
        .current_dir(seeded.path())
        .output()
        .expect("run fantastic over an existing root record");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !fdir.join("readme.md").exists(),
        "no readme without a record write"
    );
    assert!(
        !fdir.join("docs").exists(),
        "no docs/ without a record write"
    );

    // An UNPARSEABLE record is a reseed on both sides — sidecars land again.
    let broken = tempfile::TempDir::new().unwrap();
    let bdir = broken.path().join(".fantastic");
    std::fs::create_dir_all(&bdir).unwrap();
    std::fs::write(bdir.join("agent.json"), "{NOT_JSON").unwrap();
    let out = Command::new(fantastic_bin())
        .args(["kernel", "list_agents"])
        .current_dir(broken.path())
        .output()
        .expect("run fantastic over a corrupt root record");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        bdir.join("readme.md").exists(),
        "a reseed writes the readme"
    );
    assert!(
        bdir.join("docs/persistence.md").exists(),
        "and the docs tree"
    );
}

/// WARM SEED — every DRAINED root record write re-seeds `readme.md` AND the
/// `docs/` tree THROUGH the store, not only the cold bootstrap (python
/// `_persist_via_store` -> `_seed_readme_via_store`). Measured against python
/// 2026-09-21: delete `.fantastic/docs` + `readme.md` with a store wired, cause
/// a root persist, and both trees come back byte-identical. Copy-if-missing per
/// file — an operator's edit survives.
#[test]
fn a_root_persist_reseeds_the_readme_and_docs_through_the_store() {
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
    // Wire a store under the root — the provider the warm seed rides.
    run(&[
        "kernel_state",
        "create_agent",
        "parent_id=kernel",
        "handler_module=file_bridge.tools",
        "id=store",
        "root=.fantastic",
        "ingress_rule=allow_all_ingress",
    ]);
    // Wipe the whole tree AND keep one operator-edited doc to prove
    // copy-if-missing.
    std::fs::remove_dir_all(fdir.join("docs")).unwrap();
    std::fs::remove_file(fdir.join("readme.md")).unwrap();
    std::fs::create_dir_all(fdir.join("docs")).unwrap();
    std::fs::write(fdir.join("docs/reflect.md"), "MINE").unwrap();
    // A root record write — the drained persist carries the seed.
    run(&[
        "kernel",
        "update_agent",
        "id=kernel_state",
        "description=warm",
    ]);
    assert!(fdir.join("readme.md").exists(), "readme re-seeded");
    for (key, body) in fantastic_core::DOCS {
        let path = fdir.join(format!("docs/{key}.md"));
        let got = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{key}: {e}"));
        if *key == "reflect" {
            assert_eq!(got, "MINE", "an operator edit is never clobbered");
        } else {
            assert_eq!(&got, body, "{key} re-seeded byte-identical");
        }
    }
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
        tmp.path().join(".fantastic/agent.json").exists(),
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
/// warning, a corrupt `agent.json` skip), so a caller piping into `jq` broke.
#[test]
fn one_shot_stdout_is_only_the_reply_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    // A record the bundle registry cannot resolve AND a corrupt sibling: both
    // used to print a WARN line through the subscriber.
    std::fs::create_dir_all(tmp.path().join(".fantastic/agents/broken")).unwrap();
    std::fs::write(
        tmp.path().join(".fantastic/agents/broken/agent.json"),
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
        tmp.path().join(".fantastic/agents/broken/agent.json"),
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
        !stderr.contains("agent.json"),
        "python skips a corrupt record silently: {stderr}"
    );
}

/// A14.13 — "nothing to serve" (no web agent, no tty) is decided BEFORE the
/// lock and before any boot — python `_default`'s early `return` never calls
/// `FantasticLock()`. Proof: no `lock.json` is ever created, and stdout
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
        !tmp.path().join(".fantastic/lock.json").exists(),
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
    // A root record too — a VIRGIN dir (no `.fantastic/agent.json`) reseeds
    // fresh and hydrates NO children on that boot, python's own rule
    // (`bootstrap.rs`'s `boot_disk`), so a bare `agents/web/agent.json` with
    // no root record alongside it is invisible on the very first boot.
    std::fs::create_dir_all(tmp.path().join(".fantastic")).unwrap();
    std::fs::write(
        tmp.path().join(".fantastic/agent.json"),
        "{\"id\": \"kernel_state\", \"handler_module\": \"kernel_state.tools\"}",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join(".fantastic/agents/web")).unwrap();
    std::fs::write(
        tmp.path().join(".fantastic/agents/web/agent.json"),
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
/// never takes the lock.
#[test]
fn one_shot_refuses_a_live_lock_with_python_text() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".fantastic")).unwrap();
    // A process that is definitely alive for the length of this test.
    let mut holder = Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn the lock holder");
    let pid = holder.id();
    std::fs::write(
        tmp.path().join(".fantastic/lock.json"),
        format!("{{\"pid\": {pid}}}"),
    )
    .unwrap();
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
    let _ = holder.kill();
    let _ = holder.wait();
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
        .join(".fantastic/agents/store/agent.json")
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
    let rec = tmp.path().join(".fantastic").join("agent.json");
    assert!(rec.is_file(), "the usage error still seeds the root record");
    let text = std::fs::read_to_string(&rec).unwrap();
    assert!(
        text.contains("\"id\": \"kernel_state\"") && text.contains("\"callable_by\""),
        "seeded record: {text}"
    );
}

/// A14.3 — `init` against a LIVE lock prints python's `[init] …` line on
/// stderr, nothing on stdout, and exits 1.
#[test]
fn init_refuses_a_live_lock_with_python_text() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".fantastic")).unwrap();
    let mut holder = Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn the lock holder");
    let pid = holder.id();
    std::fs::write(
        tmp.path().join(".fantastic/lock.json"),
        format!("{{\"pid\": {pid}}}"),
    )
    .unwrap();
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
    let _ = holder.kill();
    let _ = holder.wait();
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
            "handler_module=text_file.tools",
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

/// #37 — a ONE-SHOT is DURABLE at exit. The ledger flusher is debounced and
/// coalescing, so the execution the process just ran is only on disk if
/// `Kernel::shutdown` lands the last window BEFORE the lock is released.
/// Same for the message log: the call row and its settle row are both there
/// once the process is gone.
#[test]
fn a_one_shot_flushes_its_ledger_and_message_log_before_exit() {
    let tmp = tempfile::TempDir::new().unwrap();
    create_store(tmp.path());
    run_cli(tmp.path(), &["kernel", "list_agents"]);

    let ledger = std::fs::read_to_string(tmp.path().join(".fantastic/ledger.json"))
        .expect(".fantastic/ledger.json must exist after a one-shot");
    let doc: serde_json::Value = serde_json::from_str(&ledger).expect("the ledger is json");
    let rows = doc["rows"].as_array().expect("the ledger has rows");
    assert!(
        rows.iter().any(|r| r["target"] == "kernel_state"),
        "the one-shot's own execution must be settled on disk: {ledger}"
    );

    let xml = std::fs::read_to_string(tmp.path().join(".fantastic/messages.xml"))
        .expect(".fantastic/messages.xml must exist after a one-shot");
    assert!(xml.contains("kind=\"call\""), "no call row: {xml}");
    assert!(xml.contains("kind=\"settle\""), "no settle row: {xml}");
}

/// #37 — a DAEMON is durable at SIGTERM. A live REST call names its own
/// execution id; after the signal the process exits 0, `lock.json` is gone,
/// and that LAST execution is in `ledger.json`. A debounced flusher that is
/// not drained inside `Kernel::shutdown` loses it.
#[test]
fn a_daemon_flushes_its_last_execution_before_the_lock_goes() {
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
    // The relay dispatches into the kernel AS ITSELF: `ingress_rule` says who
    // may call IN, `can_call` says where into the kernel it may land, and
    // root's own `callable_by` must name the grandchild ($children does not).
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
        ],
    );
    run_cli(
        tmp.path(),
        &[
            "kernel_state",
            "update_agent",
            "id=kernel_state",
            "callable_by={\"$self\": \"*\", \"$parent\": \"*\", \"$children\": \"*\", \"rest\": \"*\"}",
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
        !tmp.path().join(".fantastic/lock.json").exists(),
        "the lock must be released"
    );
    let ledger = std::fs::read_to_string(tmp.path().join(".fantastic/ledger.json"))
        .expect(".fantastic/ledger.json must exist after a daemon stop");
    assert!(
        ledger.contains(exec_id),
        "the LAST execution must be flushed before exit: {ledger}"
    );
}
