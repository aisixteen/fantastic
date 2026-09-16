//! Integration tests for the `fantastic` binary.
//!
//! Each test runs the binary in a fresh tempdir so the workdir lock
//! and on-disk state stay scoped.

use std::process::Command;

fn fantastic_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fantastic_kernel")
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
            "core",
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
            "core",
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

#[test]
fn one_shot_dispatch_returns_json_reply() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Wire the provider, then create a file agent that persists through it.
    create_store(tmp.path());
    Command::new(fantastic_bin())
        .args([
            "core",
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
    assert!(
        !stdout.contains('\u{2014}'),
        "no raw non-ascii may reach stdout: {stdout}"
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
            "core",
            "create_agent",
            "parent_id=kernel",
            "handler_module=nope.tools",
            "id=np",
        ])
        .current_dir(tmp.path())
        .output()
        .expect("run fantastic create_agent");
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<serde_json::Value>(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be ONE json document ({e}): {stdout}"));
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
    // Stripped mid-argv: the call still resolves to `core reflect`.
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(fantastic_bin())
        .args(["--ledger-cap", "5", "core", "reflect", "tree=none"])
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
        .args(["--ledger-cap", "inf", "core", "reflect", "tree=none"])
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
        .args(["core", "reflect"])
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
