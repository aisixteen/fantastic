//! Process edge of the io family — rust mirror of the canonical python
//! `io/exec_bridge` (which is canonical; on disagreement this crate is wrong).
//!
//! Crossing kernel↔subprocess is gated like any io_bridge leg: **SEALED BY
//! DEFAULT** (absent `ingress_rule` ⇒ every verb except `reflect` denies
//! `{reason:"unauthorized"}`), and bound by a **regex command ALLOWLIST**:
//! `allowed` on the record is a list of regexes, and `start {command, args}`
//! refuses unless the shell-joined line fullmatches one. No per-call override,
//! no default, no PATH fallback — an empty `allowed` runs nothing.
//!
//! A PTY `start` with no `command` runs the CHILD's own `$SHELL`, read from the
//! environment the record passes down (`inherit_env` / `env`) — autodiscovery,
//! never a default. The record's `shell` (`true` → `$SHELL`, `"login"` →
//! `$SHELL -l`) permits exactly that line without an `allowed` entry.
//!
//! It is a GENERAL-PURPOSE COMMAND runner. A program is a FILE some process (or a `write_stream`) put on disk; running it is `command` +
//! `args`, exactly like anything else — so the job table / output pump / connector
//! relay / fd passing / task-table rows / mail contract are all language-blind.
//!
//! The bridge publishes a WIRE — the inherited `FANTASTIC_CTRL_FD`, framed
//! `[4-byte BE length | kind | rest]` (T = JSON, B = raw bytes) — and nothing else. A program that wants a
//! `kernel` global includes the reference wire client ([`CONNECTOR_SRC`]) in its
//! own source; the bridge prepends nothing. `start` spawns in the BACKGROUND and
//! returns `{job_id, status:"running"}` at once, streaming `progress` / `job_done`;
//! `boot` is a no-op (nothing auto-restarts — a command is started by a caller,
//! not the record). Any verb this bundle does not own is MAIL for the running
//! program (`{queued:true}`); jobs are kernel TASKS (`ps`/`cancel`, `on_cancel`
//! SIGKILLs the real process).
//!
//! **NOT a sandbox.** An allowlisted command is a NAMED command, with all of that
//! command's reach — a python it permits still has `open()`, `socket`,
//! `subprocess`. The allowlist buys exactly one property: the OPERATOR chose WHICH
//! commands may run, and a program cannot pick a different one. Anything stronger
//! needs an OS mechanism (`sandbox-exec`, a separate uid, a container) and is
//! DEFERRED.
//!
//! ## Verbs
//!
//! | verb       | args                                                            | reply                                          |
//! |------------|-----------------------------------------------------------------|------------------------------------------------|
//! | `reflect`  | _none_ (UNGATED — discovery works on a sealed leg)              | identity + allowlist + posture                 |
//! | `start`    | `command:str` (req; PTY: absent = child `$SHELL`), `args?`, `pty?` | `{job_id, status:"running", pty}` at once   |
//! | `status`   | `job_id:int?`                                                   | one job with its output, or all of this agent's |
//! | `clear`    | `job_id:int?`                                                   | drops finished rows of this agent              |
//! | `interrupt`| `job_id:int?`                                                   | `{interrupted: int}` (SIGINT to in-flight)     |
//! | `stop`     | `job_id:int?`                                                   | `{killed: int}` (SIGKILL to in-flight)         |
//! | `boot`     | _none_                                                          | no-op → `null`                                 |
//!
//! Anything else is MAIL for the running program: `{queued:true}` when a job is
//! live, a loud error naming the verb when none is.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::{io_error_str, py_repr, py_repr_value, py_str, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{
    bad, missing, refusal, refusal_hint, with, MISSING_ARG, NOT_FOUND, TRANSPORT_ERROR,
    UNAUTHORIZED,
};
use fantastic_kernel::{body_at, AgentId, Blob, Kernel, TaskId};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// The child-side connector, VENDORED VERBATIM from python's
/// `exec_bridge/_connector.py`.
///
/// It is pure-stdlib Python (`os, json, socket, struct, threading, queue, sys`) with
/// zero dependency on the python kernel. A program that wants a `kernel` global
/// INCLUDES this text in its own source (it is a library the program bundles, not
/// something the bridge prepends). That makes the wire identical by construction rather
/// than by discipline: `[4-byte BE length | kind | rest]` (T = JSON, B = the io_bridge
/// binary frame for raw bytes), with `FANTASTIC_CTRL_FD` /
/// `FANTASTIC_SELF_ID` in the child env.
///
/// It is VENDORED rather than `include_str!`d across the tree because the container's
/// rust stage copies only `src/lib/rust/` — a cross-tree include is a hard build break
/// there. `connector_matches_python_source` in `tests.rs` is what keeps the copy honest:
/// drift fails a test rather than corrupting a run.
pub const CONNECTOR_SRC: &str = include_str!("connector.py");

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "exec_bridge.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

// ── the command gate: a REGEX allowlist (py `_allowed_patterns` / `_command_permitted`) ──

/// This leg's `allowed` — regexes a command line must fullmatch to run. A bare
/// string is treated as a one-element list. Empty ⇒ the leg runs nothing (a legal
/// state: wired but given nothing to run, refusing loudly).
pub fn allowed_patterns(meta: &Map<String, Value>) -> Vec<String> {
    match meta.get("allowed") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().map(value_to_pattern).collect(),
        _ => vec![],
    }
}

/// Mirror python's `str(x)` over each `allowed` element: a string as-is, anything
/// else via CPython's `str()` (so `True`, not `true`).
fn value_to_pattern(v: &Value) -> String {
    py_str(v)
}

/// Compile one `allowed` pattern in PYTHON's `re` dialect (A4.7 / D5).
///
/// `fancy-regex`, not `regex`: python's engine is a backtracker, so an operator's
/// allowlist may legitimately use `(?P<name>…)`, `(?P=name)`, a backreference or
/// lookaround — every one of which `regex` rejects at compile time, turning a
/// working python policy into a leg that permits nothing.
fn compile_pattern(pat: &str) -> Result<fancy_regex::Regex, fancy_regex::Error> {
    fancy_regex::Regex::new(pat)
}

/// `None` when `line` fullmatches an allowed pattern; else the refusal text.
///
/// The FULL command line (command + args, shell-joined) is matched, ANCHORED — the
/// rust equivalent of python's `re.fullmatch(pat, line, re.DOTALL)` is
/// `\A(?s:PAT)\z`: `\A`/`\z` pin both ends so `echo.*` cannot smuggle
/// `echo; rm -rf /` past a prefix, and `(?s)` makes `.` span newlines (an argv
/// element may legitimately contain them). An empty allowlist runs nothing.
pub fn command_permitted(meta: &Map<String, Value>, line: &str) -> Option<String> {
    let pats = allowed_patterns(meta);
    if pats.is_empty() {
        return Some(
            "exec_bridge: no commands are allowed on this leg — an operator must \
             set `allowed=[<regex>, ...]` naming what may run"
                .to_string(),
        );
    }
    for pat in &pats {
        // A broken pattern permits nothing; validate_meta refuses new ones.
        if let Ok(re) = compile_pattern(&format!(r"\A(?s:{pat})\z")) {
            if re.is_match(line).unwrap_or(false) {
                return None;
            }
        }
    }
    // py `f"…: {line!r} — this leg permits: {pats}"`: the line is repr'd, the list
    // printed as python prints a `list[str]`.
    Some(format!(
        "exec_bridge: command not allowed: {} — this leg permits: {}",
        py_repr(line),
        py_repr_value(&json!(pats))
    ))
}

/// Shell-join argv the way python's `shlex.join` does — POSIX quoting, so the joined
/// line the allowlist matches is the same one a shell would see. A token with only
/// shell-safe characters is passed through; anything else is single-quoted with inner
/// `'` escaped as `'"'"'`; an empty token becomes `''`.
pub fn shlex_join(argv: &[String]) -> String {
    argv.iter()
        .map(|s| shlex_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One token, quoted per python's `shlex.quote`. Safe set is `[\w@%+=:,./-]`
/// (ASCII word chars plus `@%+=:,./-`); anything else forces single-quoting.
fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
            )
    });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

// ── the child's own shell (py `SHELL_VAR` / `_shell_tail` / `_shell_permits`) ──
//
// A PTY `start` that names no `command` runs the CHILD's `$SHELL`: a client with
// no environment of its own (a browser page) cannot name the shell, but the
// record can — `inherit_env`/`env` decide what the child's environment holds,
// and `SHELL` is read from THAT, never from the kernel's own env unless it was
// passed down. One read, no chain: a child env without `SHELL` is refused loud.
//
// The ALLOWLIST still decides what may run. A record writer who does not know
// the shell's path cannot write an exact `allowed` line for it, so the record's
// `shell` field permits the resolved shell line itself — and nothing wider:
//
//   shell: true     → exactly [$SHELL]        (the bare shell)
//   shell: "login"  → exactly [$SHELL, "-l"]  (a login shell)
//
// It is a LINE, not a pattern: any other argv, and every pipe job, still has to
// fullmatch `allowed`. Who may set `shell` is who may write the record — the
// same authority that writes `allowed`.

/// The child env var a PTY `start` without `command` runs.
pub const SHELL_VAR: &str = "SHELL";

/// The argv the record's `shell` permits AFTER `$SHELL`, or `None` when the record
/// permits no shell line. Only JSON `true` and the string `"login"` count — a `1`
/// is not `true` (python compares `True` by identity for the same reason).
pub fn shell_tail(meta: &Map<String, Value>) -> Option<Vec<String>> {
    match meta.get("shell") {
        Some(Value::Bool(true)) => Some(vec![]),
        Some(Value::String(s)) if s == "login" => Some(vec!["-l".to_string()]),
        _ => None,
    }
}

/// One var of the child's environment as the CHILD will see it. python's
/// `_child_env` stringifies every `env` extra (`str(v)`), so a non-string value is
/// read the way python prints it.
fn child_var(env: &Map<String, Value>, name: &str) -> Option<String> {
    env.get(name).map(|v| match v {
        Value::String(s) => s.clone(),
        other => py_str(other),
    })
}

/// True when `argv` is exactly the shell line the record's `shell` permits: a PTY
/// job whose argv is the child's own `$SHELL` plus the tail its `shell` value
/// names. Judged on the RESOLVED argv, so it holds whether the caller named the
/// shell or left `command` out.
fn shell_permits(
    meta: &Map<String, Value>,
    env: &Map<String, Value>,
    argv: &[String],
    pty: bool,
) -> bool {
    let (Some(tail), Some(shell)) = (shell_tail(meta), child_var(env, SHELL_VAR)) else {
        return false;
    };
    if !pty || shell.is_empty() {
        return false;
    }
    let mut want = Vec::with_capacity(tail.len() + 1);
    want.push(shell);
    want.extend(tail);
    argv == want.as_slice()
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The exec_bridge bundle.
pub struct ExecBridgeBundle;

/// HOW TO CALL an exec_bridge agent — python's `VERBS` docstrings (first line
/// each), served by the kernel's `docs` verb.
fn verb_docs_json() -> Value {
    json!({
        "start": "args: command:str (req — the executable ALONE, e.g. `grep`; on a PTY start, absent = the child's $SHELL), args:list[str]? (its arguments, one item each — `<args><item>-rn</item><item>x</item></args>`), pty:{cols,rows}? (run on a terminal). The FULL line (command + args) must fullmatch one of the `allowed` regexes, or be the shell line the record's `shell` permits.",
        "status": "args: job_id:int? — one job (with full stdout/stderr), or all of this agent's jobs (summary). A job's id is its kernel task id: `ps` lists it, `cancel task_id=` kills it. Returns {jobs:[…]}.",
        "stop": "args: job_id:int? — SIGKILL one job (its whole session: the command and everything it launched) or ALL running jobs on this agent.",
        "interrupt": "args: job_id:int? — SIGINT one job or ALL running jobs on this agent.",
        "clear": "args: job_id:int? — drop finished jobs (one, or all non-running) from the RAM",
        "write": "args: job_id:int (req), data:str or bytes:bytes (exactly one). Keystrokes/paste into a PTY job, written IN FULL and serialized per job (a bracketed paste is never truncated or interleaved). `bytes` is written VERBATIM — a trailing newline (Enter) survives; a `<call>` strips a scalar value like `data`, so send keys that end in Enter as `bytes`. Returns {written:int}.",
        "resize": "args: job_id:int (req), cols:int (req), rows:int (req). Sets a PTY job's window size (the tty sends SIGWINCH, so a TUI redraws). Returns {resized:true, cols, rows}.",
        "ack": "args: job_id:int (req), chars:int (req). Flow control from a streaming consumer (an xterm acks each `output` chunk after parsing it): the reader pauses past 100K unacked chars and resumes under 5K. Returns {unacked:int, paused:bool}.",
        "output": "args: job_id:int (req), max_bytes:int? (default the whole 256 KiB ring). The PTY job's recent output — what a client re-attaching replays before the live `output` events. Returns {job_id, output:str}.",
        "tail": "args: job_id:int (req), lines:int? (default 20, max 500), stream:str? (stdout|stderr|both, default both — a PTY job has one). The last lines a job has printed SO FAR — running or ended, pipe or PTY. Returns {job_id, status, lines:[{n, stream, line}], printed, dropped}.",
        "grep": "args: job_id:int (req), match:str (req — a regex, searched in each line), lines:int? (most matches returned, default 50, max 500 — the LAST ones), stream:str? (stdout|stderr|both, default both). Searches what a job has printed SO FAR — running or ended, pipe or PTY; lines older than the 256 KiB ring are gone (`dropped` says how many). Returns {job_id, status, matches:[{n, stream, line}], total, scanned, dropped}.",
        "paste_image": "args: job_id:int (req), data:bytes (req), mime:str (req — image/png|jpeg|gif|webp). Saves a pasted image to a per-job scratch file and types its path into the PTY (a drag-drop — a program like claude in a browser xterm cannot reach the browser clipboard). Returns {path:str, bytes:int}.",
        "connector": "no args — the reference wire client SOURCE (python): paste `source` verbatim at the top of your program and it gets a `kernel` global (send/emit/watch/on_message); returns {source, howto}.",
        "monitor": "args: job_id:int (req — a RUNNING job of this agent), notify:str (req — the agent told), match:str? (a regex; absent = every line), every:number? (seconds — at most one delivery per window; required without `match`), tail:int? (newest lines per delivery, default 100), label:str?. Watches the job's output AS IT ARRIVES and SENDS what matches to `notify` as a turn — it polls nothing. Returns {monitor_id, job_id, notify, match, every, tail, label}; kernel `cancel` (or `unmonitor`) ends it, and it ends with its job.",
        "monitors": "no args — this agent's live monitors: {monitors:[{monitor_id, job_id, notify, match, every, tail, label, delivered, pending}]}.",
        "unmonitor": "args: monitor_id:int (req). End one of this agent's monitors (kernel `cancel` on its id does the same). Returns {unmonitored:true, monitor_id}.",
        "reflect": "Identity + running job count + auth posture. No args.",
        "boot": "No-op. Nothing auto-restarts: a command is started by a caller, not by the record.",
    })
}

/// What a job emits on this agent's inbox — python `EMITS`, served by `docs`.
fn emits_json() -> Value {
    json!({
        "job_started": "{type:'job_started', job_id, command, args} — the RESOLVED invocation (a PTY start without `command` names the shell it ran)",
        "progress": "{type:'progress', job_id, n, line, stream} — one per output line",
        "job_done": "{type:'job_done', job_id, exit_code, ok, stdout?, stderr?} — a PTY job carries no stdout/stderr split",
        "output": "{type:'output', job_id, data} — a PTY job's output chunk (UTF-8)",
    })
}

/// How a running program talks back — python `NOTES`, served by `docs`.
fn notes_json() -> Value {
    json!({
        "wire": "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD, a BIDIRECTIONAL socket: frame is [4-byte BE length | kind | rest], both directions — kind T: a JSON envelope; kind B: [4-byte BE H | H-byte JSON header | raw body], the header's `_binary_path` naming where the body belongs as steps from the frame root — a key as a string (taken literally: ['payload', 'a.b'] is the key 'a.b'), a list index as an int (a stream chunk rides raw, never base64). To CALL another agent: write {op:'send', rid:<your id>, target, payload}; the reply COMES BACK on the SAME fd as {op:'reply', rid:<same id>, data}. Read frames in a loop and skip any whose rid isn't yours (event/inbox frames can arrive first). ops: send/emit/reflect/watch/on_message. A command that never reads the fd runs like any OS process. The `connector` verb returns a ready client.",
        "inbox": "Any verb this bundle does not answer is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is refused — a script that only defines something exits before its first message arrives.",
        "tasks": "Every running job IS a row in the kernel TASK TABLE: its `job_id` is that row's id, listed by `ps`. `cancel task_id=<job_id>` kills it — the OS process is SIGKILLed by this bundle's `on_cancel` hook first, so nothing is orphaned.",
    })
}

#[async_trait]
impl Bundle for ExecBridgeBundle {
    fn name(&self) -> &str {
        "exec_bridge"
    }

    fn verb_docs(&self) -> Value {
        verb_docs_json()
    }

    fn emits_doc(&self) -> Option<Value> {
        Some(emits_json())
    }

    fn notes_doc(&self) -> Option<Value> {
        Some(notes_json())
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// Refuse a write this bundle cannot honour, at the WRITE, next to the typo
    /// (py-canonical `validate_meta`): the ingress posture, the record's `cwd` /
    /// `env` / `inherit_env` shapes, `shell` (true / false / "login"), and
    /// `allowed` as a non-empty list of compilable regexes.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        // ONLY an UNKNOWN RULE NAME is refused (py `resolve_ingress(...).unknown`):
        // a falsy spec and a type-less object both seal silently, and
        // `password_ingress` resolves fine here — it is the stem check below that
        // refuses it on this edge.
        if let Some(bad) = fantastic_io_bridge::authorizer::ingress::unknown_refusal(
            meta,
            &["allow_all_ingress", "deny_ingress"],
        ) {
            return Some(bad);
        }
        // `password_ingress` is meaningless on an in-process leg: the token is
        // read off a WIRE ENVELOPE and the process edge has none — no frame ever
        // presents one, so the rule could only refuse, or (until 2026-09-02) be
        // satisfied by a secret persisted ON THE RECORD. Refused at the write
        // (#116). Same rule as `fantastic_file::password_refusal`.
        for (field, stem) in [("ingress_rule", "password_ingress"), ("auth", "password")] {
            let Some(spec) = meta.get(field) else {
                continue;
            };
            if fantastic_io_bridge::authorizer::rule_name(Some(spec), "") == stem {
                return Some(format!(
                    "{field}={} is not applicable to the process edge: a leg with \
                     no wire envelope can never present a token — legal values: \
                     allow_all_ingress, deny_ingress",
                    py_repr_value(spec)
                ));
            }
        }
        // WHERE and WITH WHAT a command runs is the record's — shaped at the write.
        if let Some(c) = meta.get("cwd").filter(|v| !v.is_null()) {
            if !c.as_str().is_some_and(|s| !s.is_empty()) {
                return Some("cwd must be a non-empty directory path".to_string());
            }
        }
        if let Some(e) = meta.get("env").filter(|v| !v.is_null()) {
            let ok = e.as_object().is_some_and(|m| {
                m.values()
                    .all(|v| matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)))
            });
            if !ok {
                return Some("env must be a map of NAME -> value".to_string());
            }
        }
        if let Some(n) = meta.get("inherit_env").filter(|v| !v.is_null()) {
            let ok = n
                .as_array()
                .is_some_and(|a| a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty())));
            if !ok {
                return Some(
                    r#"inherit_env must be a list of env var names (["*"] = all)"#.to_string(),
                );
            }
        }
        // `shell` is a CLOSED set — each legal value names ONE exact line. A JSON
        // `1` is refused: python checks `isinstance(v, bool)` first so a bare `1`
        // (equal to True there) can never read as permission.
        if let Some(v) = meta.get("shell").filter(|v| !v.is_null()) {
            let ok = matches!(v, Value::Bool(_)) || v.as_str() == Some("login");
            if !ok {
                return Some(format!(
                    "shell={} is not a shell permission — legal values: true \
                     (the child's $SHELL, bare), \"login\" ($SHELL -l), false (none)",
                    py_repr_value(v)
                ));
            }
        }
        // `allowed` must be a non-empty list of compilable regexes.
        if let Some(raw) = meta.get("allowed").filter(|v| !v.is_null()) {
            let pats: Vec<String> = match raw {
                Value::String(s) => vec![s.clone()],
                Value::Array(a) => a.iter().map(value_to_pattern).collect(),
                _ => Vec::new(),
            };
            if pats.is_empty() {
                return Some(
                    "allowed must be a non-empty list of regexes, e.g. \
                     <allowed><item>ls( .*)?</item></allowed>"
                        .to_string(),
                );
            }
            // py reprs the RAW element and compiles `str(pat)`; the engine's own
            // message after `': '` is the D4/D5 residual.
            let raw_items: Vec<Value> = match raw {
                Value::Array(a) => a.clone(),
                other => vec![other.clone()],
            };
            for (item, pat) in raw_items.iter().zip(&pats) {
                if let Err(e) = compile_pattern(pat) {
                    return Some(format!(
                        "allowed contains a broken regex {}: {e}",
                        py_repr_value(item)
                    ));
                }
            }
        }
        None
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let raw_type = payload.get("type").unwrap_or(&Value::Null);
        let verb = raw_type.as_str().unwrap_or("");
        // AUTH GATE — the process edge, gated like any io_bridge leg (SEALED by
        // default). `reflect` and `connector` are UNGATED: discovery must work on a
        // sealed bridge. Every other type is gated, MAIL INCLUDED (python `handler`,
        // audit BV-8): a sealed leg delivers nothing to its program — the relay's
        // inbox drain refuses the same payloads, since the substrate's fanout
        // reaches it before this handler runs.
        //
        // `shutdown` is NOT a verb (A4.3): python's VERBS has no such key, so it is
        // MAIL like any other unowned type.
        if !matches!(verb, "reflect" | "connector") {
            let meta = snapshot_meta(agent_id, kernel);
            if let Some(denied) = gate(&meta, agent_id, verb) {
                return Ok(Some(denied));
            }
        }
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, kernel),
            "start" => start_reply(agent_id, payload, kernel).await,
            "boot" => boot_reply(agent_id, kernel).await,
            "status" => status_reply(agent_id, payload),
            "clear" => clear_reply(agent_id, payload),
            "interrupt" => interrupt_reply(agent_id, payload),
            "stop" => stop_reply(agent_id, payload),
            "write" => pty::write_reply(agent_id, payload, None).await,
            "resize" => pty::resize_reply(agent_id, payload),
            "ack" => pty::ack_reply(agent_id, payload),
            "output" => pty::output_reply(agent_id, payload),
            "tail" => tail::tail_reply(agent_id, payload),
            "grep" => tail::grep_reply(agent_id, payload),
            "monitor" => monitor::monitor_reply(agent_id, payload, kernel).await,
            "monitors" => monitor::monitors_reply(agent_id),
            "unmonitor" => monitor::unmonitor_reply(agent_id, payload, kernel).await,
            // `data` is bytes: on the text channel there is none to paste.
            "paste_image" => pty::paste_image_text(agent_id, payload),
            // The reference wire client SOURCE — a model pastes it at the top of
            // its program instead of re-deriving the socket framing (measured
            // 2026-09-01, E30 subai:rust: ten messages lost to int(fd)/buffering/
            // not-seekable before the first working frame). Ungated like reflect.
            "connector" => json!({
                "source": CONNECTOR_SRC,
                "howto": "Write `source` verbatim as the first lines of your program file, then \
                          your code below it. It opens FANTASTIC_CTRL_FD (a socket, length-prefixed \
                          frames; a payload carrying a bytes value rides the binary channel raw: \
                          `write_stream` puts its chunk in `bytes`, `read_stream` passes `bytes: b''` \
                          and gets its chunk back raw in the reply's `bytes`) and \
                          exposes `kernel.send(target, payload) -> reply` (raises if \
                          the relay could not deliver), \
                          `kernel.emit(payload)`, `kernel.watch(src, cb)`, `kernel.on_message(cb)`, \
                          `kernel.self_id`. Arguments arrive as ARGV, like any command — there is \
                          no `kernel.args`. One frame caps at 16 MiB; move more than that with \
                          `write_stream` chunks. Your sends carry THIS runner's identity \
                          and grants (its `can_call`; a target must list this runner in \
                          `callable_by`). Do not rewrite the wire by hand.",
            }),
            // ADDRESSABLE INBOX. Any verb this bundle does not own is mail for the
            // running program — the substrate's fanout has ALREADY delivered it to the
            // job's `_xbself_` proxy, so this only ACKs. Re-emitting would double it.
            _ => {
                if jobs::has_running_job(agent_id) {
                    json!({"queued": true})
                } else {
                    // py `{t!r}` over the RAW value — an absent `type` is `None`.
                    let mut r = fantastic_kernel::refusal::refusal(
                        fantastic_kernel::refusal::UNHANDLED,
                        format!(
                            "exec_bridge: unknown type {} (no running job to receive it)",
                            py_repr_value(raw_type)
                        ),
                    );
                    fantastic_kernel::refusal::with(&mut r, "verb", raw_type.clone());
                    r
                }
            }
        };
        Ok(Some(reply))
    }

    /// The kernel's dispatch, every verb: `paste_image` reads ITS key — the
    /// request body at `data` (python's `payload.get("data")` is the bytes). A
    /// body under any other key, or none, is not the image: python's `data` is
    /// then absent, `bad_arg` "raw bytes". Every other verb takes `handle`.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        if header.get("type").and_then(Value::as_str) == Some("paste_image") {
            let meta = snapshot_meta(agent_id, kernel);
            if let Some(denied) = gate(&meta, agent_id, "paste_image") {
                return Ok((Some(denied), None));
            }
            let reply = match body_at(body.as_ref(), "data") {
                Some(data) => pty::paste_image_reply(agent_id, &header, data).await,
                None => pty::paste_image_text(agent_id, &header),
            };
            return Ok((Some(reply), None));
        }
        if header.get("type").and_then(Value::as_str) == Some("write") {
            let meta = snapshot_meta(agent_id, kernel);
            if let Some(denied) = gate(&meta, agent_id, "write") {
                return Ok((Some(denied), None));
            }
            let reply = pty::write_reply(agent_id, &header, body_at(body.as_ref(), "bytes")).await;
            return Ok((Some(reply), None));
        }
        Ok((self.handle(agent_id, &header, kernel).await?, None))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        // Every job of this agent, so the dir can be rmtreed without leaking children.
        {
            let mut g = jobs::jobs();
            for j in g.values_mut().filter(|j| &j.owner == agent_id) {
                jobs::teardown(j);
            }
        }
        Ok(())
    }
    /// Shutdown frees what delete frees — declared, never a fallback (audit c9).
    async fn on_shutdown(
        &self,
        agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        self.on_delete(agent_id, kernel).await
    }

    /// A task of this agent is being cancelled — SIGKILL its subprocess FIRST.
    /// A job's child is spawned with `kill_on_drop(false)`, so dropping the
    /// future would leave a live `python` process with no parent and no owner:
    /// an orphan. Aborting a future frees no OS resource; this hook does.
    ///
    /// TASK-SCOPED. `cancel <task_id>` must kill THAT job, not every child of the
    /// agent — a durable tool and a one-shot job can be in flight together, and
    /// cancelling one used to take out the other.
    ///
    /// The kernel runs every `on_cancel` in the subtree BEFORE aborting any future,
    /// so the OS process dies before its pump is dropped. Cancelling an
    /// `asyncio`-style task frees no real resource; this is the half that does.
    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &TaskId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let mut g = jobs::jobs();
        // A job's id IS its pump's task id, so the key names the job to kill.
        if let Some(j) = g.get_mut(&(agent_id.clone(), *task_id)) {
            jobs::teardown(j);
        }
        drop(g);
        Ok(())
    }
}

// ── the auth gate (io_bridge leg — SEALED by default) ───────────────

/// Deny an owned verb on a sealed leg. Same shape as `fantastic-file`'s gate:
/// absent `ingress_rule` (or `auth`) resolves to deny-inbound, and every denial
/// teaches the fix. `kind="spawn"` — that is what is being authorized; no rule
/// inspects the kind, so a new one costs nothing and the audit trail is honest.
///
/// The reply FORWARDS the rule's own `Denial` — reason, hint, see. Hand-rolling a
/// hint here made one sealed leg teach a different repair per bundle and per
/// runtime; the rule that refused is the thing that knows the repair.
fn gate(meta: &Map<String, Value>, agent_id: &AgentId, verb: &str) -> Option<Value> {
    let spec = fantastic_io_bridge::authorizer::manifold_spec(
        meta.get("ingress_rule"),
        meta.get("auth"),
        "_ingress",
    );
    let rule = fantastic_io_bridge::authorizer::ingress::resolve(spec.as_ref());
    let action = fantastic_io_bridge::authorizer::Action {
        kind: "spawn",
        target: agent_id.0.as_str(),
        verb,
        // NO CREDENTIAL (A4.16). A token is a WIRE-ENVELOPE fact and the process
        // edge has no envelope; reading one off the RECORD would let a leg satisfy
        // its own password rule with a secret it stores itself. python's
        // `gate_inbound` here is handed an Action with `token=None`.
        token: None,
    };
    if let fantastic_io_bridge::authorizer::Decision::Deny(d) =
        fantastic_io_bridge::gate_inbound(&rule, &action)
    {
        let mut out = refusal(UNAUTHORIZED, d.reason);
        if !d.hint.is_empty() {
            with(&mut out, "hint", json!(d.hint));
        }
        if !d.see.is_empty() {
            with(&mut out, "see", json!(d.see));
        }
        return Some(out);
    }
    None
}

// ── snapshot helpers ────────────────────────────────────────────────

/// Snapshot the agent record's meta map. Returns an empty map if the
/// agent has been deleted concurrently.
fn snapshot_meta(agent_id: &AgentId, kernel: &Kernel) -> Map<String, Value> {
    match kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        Some(a) => a.meta.read().expect("meta poisoned").clone(),
        None => Map::new(),
    }
}

// ── verb implementations ────────────────────────────────────────────

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    // py `rec.get("cwd")` — the record's, reported as written; null when unset.
    let cwd_val = meta.get("cwd").cloned().unwrap_or(Value::Null);
    // Liveness = the OS process has not exited — ONE definition (`jobs::alive`).
    let running = jobs::jobs()
        .values()
        .filter(|j| &j.owner == agent_id && jobs::alive(j))
        .count();
    let mut out = json!({
        "id": agent_id.as_str(),
        // Py-canonical sentence — the ALLOWLIST is advertised so an authoring
        // model can see what it may run without guessing.
        "sentence": "Process edge of the io family — a general-purpose COMMAND runner, sealed by default, gated by the `allowed` regexes. Spawns commands in the background; a child may speak the kernel over a private control fd.",
        "cwd": cwd_val,
        // THE GATE, advertised: a caller must see what it may run, where, and with
        // which of the kernel's env vars.
        "allowed": allowed_patterns(&meta),
        "inherit_env": meta.get("inherit_env").cloned().unwrap_or_else(|| json!([])),
        // The shell line the record permits without an `allowed` entry: false
        // (none), true ($SHELL bare) or "login" ($SHELL -l). py `rec.get("shell")
        // or False` — any falsy value reads as false.
        "shell": meta.get("shell").filter(|v| py_truthy(v)).cloned().unwrap_or(json!(false)),
        "running": running,
    });
    // The auth POSTURE, python's `describe(rec)` — `**_describe(rec)` is the last
    // thing python's reflect spreads, so these keys land last and in its order.
    if let (Some(o), Some(posture)) = (
        out.as_object_mut(),
        fantastic_io_bridge::authorizer::describe(&meta).as_object(),
    ) {
        for (k, v) in posture {
            o.insert(k.clone(), v.clone());
        }
    }
    out
}

pub mod jobs;
mod monitor;
pub mod pty;
pub mod relay;
pub mod spawn;
mod tail;

#[cfg(test)]
mod pty_tests;
#[cfg(test)]
mod shell_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_monitor;

/// `interrupt` / `stop` — signal this agent's jobs, or one named job.
///
/// Both now address the JOB TABLE. There is exactly one registry: a private pid set
/// alongside it would be a second truth, and `start`'s jobs would be invisible to it.
fn signal_jobs(agent_id: &AgentId, want: Option<&JobRef>, sig: nix::sys::signal::Signal) -> u64 {
    let mut g = jobs::jobs();
    let mut n = 0u64;
    for ((_, id), j) in g.iter_mut() {
        if &j.owner != agent_id || want.is_some_and(|w| w.id != Some(*id)) {
            continue;
        }
        if !jobs::alive(j) {
            continue; // already gone — never pretend
        }
        let signalled = if sig == nix::sys::signal::Signal::SIGKILL {
            j.status = "stopping".to_string();
            jobs::kill(j)
        } else {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(j.pid as i32), sig).is_ok()
        };
        if signalled {
            n += 1;
        }
    }
    n
}

/// The `not_found` refusal for a named job this agent does not own, or `None`.
///
/// A `job_id` naming nothing used to answer `{killed:0}`/`{interrupted:0}` — the
/// same reply as "it had already exited", so a typo read as success. `status`
/// answered a bare `{error}` with no code at all, and a caller branching on
/// `reason == "not_found"` (what the corpus and the e2e scenarios pin) never
/// matched on this runtime.
fn no_job(agent_id: &AgentId, want: Option<&JobRef>) -> Option<Value> {
    let jid = want?;
    match jid.find(&jobs::jobs(), agent_id) {
        None => Some(no_such_job(jid)),
        Some(_) => None,
    }
}

/// A job named by a caller (python `_job_arg`'s result): the value AS SENT — every
/// refusal echoes it — and the task id it names.
pub(crate) struct JobRef {
    /// `payload["job_id"]`, verbatim.
    pub(crate) raw: Value,
    /// The kernel task id it names; `None` for an integer too wide to be one.
    /// python's `int` is unbounded: such a value passes the type check and then
    /// simply names no job — it is not a bad argument.
    pub(crate) id: Option<TaskId>,
}

impl JobRef {
    /// This agent's row for the named job, when there is one. A job of ANOTHER
    /// agent is not found, exactly like a job that never existed.
    pub(crate) fn find<'a>(
        &self,
        g: &'a indexmap::IndexMap<jobs::JobKey, jobs::Job>,
        owner: &AgentId,
    ) -> Option<&'a jobs::Job> {
        self.id.and_then(|id| g.get(&(owner.clone(), id)))
    }
}

/// The `not_found` refusal naming `jid` as it was sent (python
/// `refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)`).
pub(crate) fn no_such_job(jid: &JobRef) -> Value {
    let mut v = refusal(
        NOT_FOUND,
        format!("exec_bridge: no job {}", py_repr_value(&jid.raw)),
    );
    with(&mut v, "job_id", jid.raw.clone());
    v
}

/// The `job_id` argument (python `_job_arg`): `Ok(None)` when absent (or `null`)
/// and optional, `Ok(Some)` for an int, or the refusal.
///
/// A job's id IS its kernel TASK id — an int. Anything else is a bad argument
/// naming that, never looked up: a string like `"42"` is not coerced, so a caller
/// that stringified an id learns so here instead of reading `no job`. (The ids
/// were random 8-hex strings; a model's `<job_id>04203581</job_id>` came back an
/// int and `458e5912` an infinite float, so 3.7% of jobs were unaddressable.)
pub(crate) fn job_arg(
    verb: &str,
    payload: &Value,
    required: bool,
) -> Result<Option<JobRef>, Value> {
    match payload.get("job_id") {
        None | Some(Value::Null) if required => Err(missing(verb, "job_id")),
        None | Some(Value::Null) => Ok(None),
        Some(v @ Value::Number(n)) if is_int(n) => Ok(Some(JobRef {
            raw: v.clone(),
            id: n.as_i64().map(TaskId),
        })),
        Some(_) => Err(bad(
            verb,
            "job_id",
            "an int — the job's task id (from `start`, `status` or `ps`)",
        )),
    }
}

/// [`job_arg`] for a verb that needs one job.
pub(crate) fn require_job(verb: &str, payload: &Value) -> Result<JobRef, Value> {
    job_arg(verb, payload, true)?.ok_or_else(|| missing(verb, "job_id"))
}

/// python `isinstance(v, int)` for a JSON number: digits with an optional sign —
/// no fraction, no exponent, and not the `Infinity`/`NaN` token the kernel keeps
/// for a non-finite float (a model's `458e5912` is one).
fn is_int(n: &serde_json::Number) -> bool {
    let s = n.to_string();
    let digits = s.strip_prefix('-').unwrap_or(&s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn interrupt_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let want = match job_arg("interrupt", payload, false) {
        Ok(w) => w,
        Err(e) => return e,
    };
    no_job(agent_id, want.as_ref()).unwrap_or_else(|| {
        json!({"interrupted": signal_jobs(agent_id, want.as_ref(), nix::sys::signal::Signal::SIGINT)})
    })
}

fn stop_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let want = match job_arg("stop", payload, false) {
        Ok(w) => w,
        Err(e) => return e,
    };
    no_job(agent_id, want.as_ref()).unwrap_or_else(|| {
        json!({"killed": signal_jobs(agent_id, want.as_ref(), nix::sys::signal::Signal::SIGKILL)})
    })
}

/// `start` — spawn the COMMAND in the BACKGROUND and return at once.
///
/// args: `command:str` (req — on a PTY start, absent = the child's `$SHELL`),
/// `args:list[str]?` (argv, default `[]`), `pty:{cols,rows}?` (run it on a
/// terminal). The FULL line (command + args, shell-joined) must fullmatch one of
/// the record's `allowed` regexes — or be the exact shell line the record's
/// `shell` permits — or the start is refused naming the patterns. A pipe job
/// always names its command. Everything else is the RECORD's, never the call's:
/// `cwd` (required), `env` + `inherit_env` (the child's environment).
async fn start_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    // EVERY refusal on this verb carries a CODE, so nothing has to tell a typo
    // from a policy refusal by matching prose.
    // WHERE and WITH WHAT a command runs are the OPERATOR's: a per-call `env`
    // (`LD_PRELOAD`, `PATH`, …) or `cwd` changed what an allowlisted line actually
    // ran (audit 2026-09-28).
    for gone in ["env", "cwd"] {
        if payload.get(gone).is_some_and(|v| !v.is_null()) {
            return bad(
                "start",
                gone,
                &format!("absent — `{gone}` is the record's (update_agent {agent_id} {gone}=…)"),
            );
        }
    }
    // `args` — a list of scalars, each stringified into argv. python's
    // `isinstance(a, (str, int, float))` ACCEPTS a bool (a bool IS an int there), and
    // `str(True)` is `True`; an explicit `null` is not a list and refuses.
    let raw_args = payload.get("args").cloned().unwrap_or_else(|| json!([]));
    let args: Vec<String> = match &raw_args {
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                match v {
                    Value::String(_) | Value::Number(_) | Value::Bool(_) => out.push(py_str(v)),
                    _ => return bad("start", "args", "a list of strings"),
                }
            }
            out
        }
        _ => return bad("start", "args", "a list of strings"),
    };
    let dims = match payload.get("pty").filter(|v| !v.is_null()) {
        None => None,
        Some(raw) => match pty::dims("start", raw) {
            Ok(d) => Some(d),
            Err(e) => return e,
        },
    };
    // The child's WHOLE environment, from the record — built before the command
    // is resolved, because an absent `command` on a PTY start is read from it.
    let mut env = child_env(&meta);
    // py `payload.get("command")`: an absent key and an explicit `null` are the
    // same "no command".
    let named = payload.get("command").filter(|v| !v.is_null());
    let command = match named {
        // AUTODISCOVERY, not a default: the shell is whatever the record passed
        // down to the child as SHELL. No chain (zsh → bash → sh) behind it — a
        // child env without one is refused, naming where it comes from.
        None if dims.is_some() => match child_var(&env, SHELL_VAR).filter(|s| !s.is_empty()) {
            Some(shell) => Value::String(shell),
            None => {
                let mut v = refusal_hint(
                    MISSING_ARG,
                    format!(
                        "exec_bridge: start named no command and the child's environment \
                         has no {SHELL_VAR} to run — a PTY start without `command` runs \
                         the shell the record passes down, never a guessed one"
                    ),
                    format!(
                        "update_agent {agent_id} inherit_env=[\"{SHELL_VAR}\"] \
                         (or env={{\"{SHELL_VAR}\": …}}), or name `command`"
                    ),
                );
                with(&mut v, "arg", json!(SHELL_VAR));
                return v;
            }
        },
        None => Value::Null,
        Some(v) => v.clone(),
    };
    // py `not isinstance(command, str) or not command.strip()`: a string of nothing
    // but WHITESPACE is missing, and `str.strip()` strips every python space
    // character, not just ASCII. A pipe job always names its command; so does a
    // PTY start that passed one explicitly (an empty string is a typo, not "use
    // the shell").
    let command = match command.as_str() {
        Some(c) if c.chars().any(|ch| !fantastic_bundle::is_space(ch)) => c.to_string(),
        _ => return missing("start", "command"),
    };
    // The FULL argv, shell-joined, matched against the allowlist BEFORE spawning.
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(command.clone());
    argv.extend(args.iter().cloned());
    let line = shlex_join(&argv);
    // Two ways a line is permitted, both written on the RECORD: the shell line
    // its `shell` field names, or a fullmatch against `allowed`. Checked in that
    // order only because the first is an exact comparison; neither widens the
    // other.
    let why = if shell_permits(&meta, &env, &argv, dims.is_some()) {
        None
    } else {
        command_permitted(&meta, &line)
    };
    if let Some(mut why) = why {
        // An AUTHORIZATION denial, not a bad argument: the command line is
        // well-formed and the LEG refused it. Same code the ingress gate answers,
        // so one branch covers "the operator has not opened this".
        if let Some(tail) = shell_tail(&meta) {
            // Say what `shell` DOES permit, so a caller that passed extra args (or
            // a pipe job) learns why the shell permission did not cover it. The
            // value is JSON (`true`/`"login"`), the wire's own spelling, so every
            // runtime prints the same bytes; the line is python's repr of the
            // shell-joined argv, `'$SHELL'` when the child has none.
            let shell = child_var(&env, SHELL_VAR)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("${SHELL_VAR}"));
            let mut permitted = vec![shell];
            permitted.extend(tail);
            why.push_str(&format!(
                " (`shell={}` permits only the PTY line {})",
                serde_json::to_string(meta.get("shell").unwrap_or(&Value::Null))
                    .unwrap_or_default(),
                py_repr(&shlex_join(&permitted))
            ));
        }
        return refusal_hint(
            UNAUTHORIZED,
            why,
            "the leg's `allowed` policy decides; an operator may widen it",
        );
    }
    let Some(cwd) = meta
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
    else {
        let mut v = refusal_hint(
            MISSING_ARG,
            "exec_bridge: this leg names no `cwd` — where a command runs is the operator's \
             decision, never guessed",
            format!("update_agent {agent_id} cwd=<dir>"),
        );
        with(&mut v, "arg", json!("cwd"));
        return v;
    };

    let spawned = match dims {
        None => crate::spawn::spawn_job(&argv, agent_id.as_str(), &env, &cwd).map(|s| (s, None)),
        Some((cols, rows)) => {
            env.insert("TERM".to_string(), json!("xterm-256color"));
            crate::spawn::spawn_pty(&argv, agent_id.as_str(), &env, &cwd, cols, rows).and_then(
                |(s, master)| {
                    set_nonblocking(&master)?;
                    Ok((s, Some(pty::Pty::new(master, cols, rows)?)))
                },
            )
        }
    };
    let (spawned, pty_state) = match spawned {
        Ok(s) => s,
        Err(e) if e.get_ref().is_some_and(|r| r.is::<crate::spawn::NoPath>()) => {
            let mut v = refusal_hint(
                MISSING_ARG,
                format!(
                    "exec_bridge: {} is a bare command and this leg gives the child no PATH \
                     to find it in — the kernel's own PATH is not the child's",
                    py_repr(&command)
                ),
                format!(
                    "update_agent {agent_id} inherit_env=[\"PATH\"] (or env={{\"PATH\": …}}), \
                     or start an absolute path"
                ),
            );
            with(&mut v, "arg", json!("PATH"));
            return v;
        }
        Err(e) => {
            // py `f"exec_bridge: spawn failed: {e}"` over the OSError
            // `create_subprocess_exec` raises — `[Errno 2] No such file or
            // directory: 'nope'`, the COMMAND as the filename. A MISSING `cwd` is
            // named instead, as CPython names it (its `noexec:chdir` path): measured
            // 2026-10-06 (rust subai), a leg whose cwd did not exist answered
            // `No such file or directory: 'python3'` and the model spent twenty
            // turns hunting an interpreter that was there all along.
            let culprit = if e.kind() == std::io::ErrorKind::NotFound
                && !std::path::Path::new(&cwd).is_dir()
            {
                &cwd
            } else {
                &command
            };
            return refusal(
                TRANSPORT_ERROR,
                format!(
                    "exec_bridge: spawn failed: {}",
                    io_error_str(&e, std::path::Path::new(culprit))
                ),
            );
        }
    };
    let crate::spawn::Spawned { child, ctrl } = spawned;
    let Some(pid) = child.id() else {
        return refusal(
            TRANSPORT_ERROR,
            "exec_bridge: child exited before it could be tracked",
        );
    };
    let (rd, wr) = ctrl.into_split();
    // THE PROGRAM'S SENDS ARE THIS AGENT'S SENDS — `spawn_task` carries the
    // dispatch identity into the relay. A `tokio::spawn` inherits no task-local,
    // so the relay used to forward frames with NO sender, i.e. as the OPERATOR,
    // ungated (measured live 2026-09-01, E30 subai:rust: a spawned program
    // reached any agent). The runner's `can_call` bounds its programs.
    // The relay's stop signal, held on the job row. `teardown` raises it instead of
    // aborting this task, so the relay leaves by its own read loop and the proxy
    // release after that loop actually runs.
    let stop = Arc::new(tokio::sync::Notify::new());

    // THE PUMP IS THE JOB'S TASK, and the job's id IS that task's id — minted by
    // the kernel's one counter when the pump is forked (python `_start`). One number
    // names the job everywhere: `status`/`stop`/`write` take it as `job_id`, `ps`
    // lists it, `cancel task_id=` kills it. (It used to be a random 8-hex string,
    // which a model's `<job_id>04203581</job_id>` or `458e5912` coerced into an int
    // or an infinite float — 3.7% of jobs could not be addressed again.)
    //
    // The id exists only once the fork does, and a forked task may already be
    // running on another worker, so the pump learns its id through `ready` — sent
    // only after the row is filed below. The pump takes everything else BY VALUE:
    // it never reads its own row before it is told the row exists.
    let (ready_tx, ready) = tokio::sync::oneshot::channel();
    let owner = agent_id.clone();
    let k = Arc::clone(kernel);
    let job_id = match &pty_state {
        None => kernel.spawn_task(agent_id, "start", async move {
            jobs::pump(owner, ready, child, k).await;
        }),
        Some(p) => {
            let p = Arc::clone(p);
            kernel.spawn_task(agent_id, "start", async move {
                pty::pump(owner, ready, child, p, k).await;
            })
        }
    };
    // REGISTERED: the relay outlives this `start` dispatch and issues a send per
    // frame the program writes, so it belongs in `ps` and within reach of
    // `cancel` like the pump above. Forked AFTER the pump, as python does, so the
    // job's id is the lower of the two.
    let ctrl_task = kernel.spawn_task(
        agent_id,
        "relay",
        crate::relay::serve(
            rd,
            wr,
            agent_id.clone(),
            job_id,
            Arc::clone(kernel),
            Arc::clone(&stop),
            None,
        ),
    );
    jobs::jobs().insert(
        (agent_id.clone(), job_id),
        jobs::Job {
            owner: agent_id.clone(),
            pid,
            ctrl: ctrl_task,
            stop,
            // WHAT ran — so a caller can re-issue this job verbatim.
            command: command.clone(),
            args: args.clone(),
            cwd: json!(cwd),
            status: "running".to_string(),
            started_at: now_secs(),
            exit_code: None,
            lines: 0,
            last_line: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            line_ring: std::collections::VecDeque::new(),
            line_ring_bytes: 0,
            line_ring_dropped: 0,
            pty: pty_state.clone(),
        },
    );
    // The row exists: the pump may look for it now. It cannot have gone away —
    // nothing drops a pump before it has received its id.
    let _ = ready_tx.send(job_id);
    // The RESOLVED invocation rides the announcement: a client that let the
    // kernel pick the shell learns which one ran, and can re-issue it verbatim.
    kernel
        .emit(
            agent_id,
            json!({"type": "job_started", "job_id": job_id.get(), "command": command, "args": args}),
        )
        .await;
    json!({"job_id": job_id.get(), "status": "running", "pty": pty_state.is_some()})
}

/// The child's WHOLE environment, from the record (python `_child_env`): the
/// kernel env vars named in `inherit_env` (nothing by default; `["*"]` = every one —
/// the operator's wildcard), then the operator's `env` extras. Nothing else leaks
/// in — the kernel's `.env` keys reach a child only when the operator names them.
/// The child's whole environment from a record: `inherit_env` names (`["*"]` =
/// all), then the record's `env` — shared with py_agent.
pub fn child_env(meta: &Map<String, Value>) -> Map<String, Value> {
    let names: Vec<String> = meta
        .get("inherit_env")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut env = Map::new();
    if names.iter().any(|n| n == "*") {
        for (k, v) in std::env::vars() {
            env.insert(k, json!(v));
        }
    } else {
        for n in &names {
            if let Ok(v) = std::env::var(n) {
                env.insert(n.clone(), json!(v));
            }
        }
    }
    if let Some(extra) = meta.get("env").and_then(Value::as_object) {
        for (k, v) in extra {
            env.insert(k.clone(), v.clone());
        }
    }
    env
}

fn set_nonblocking(fd: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: fcntl on an fd we own.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `boot` — a NO-OP returning null. Nothing auto-restarts: a command is started by a
/// caller, not by the record (py `_boot`).
async fn boot_reply(_agent_id: &AgentId, _kernel: &Arc<Kernel>) -> Value {
    Value::Null
}

/// `status` — one job (full output) or all of THIS agent's jobs.
fn status_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let want = match job_arg("status", payload, false) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let g = jobs::jobs();
    match want {
        Some(jid) => match (jid.id, jid.find(&g, agent_id)) {
            (Some(id), Some(j)) => json!({
                "jobs": [jobs::snap(id, j, true)],
                "origin": format!(
                    "{agent_id} status {{job_id: {}}} — a live job; re-send for its \
                     output NOW, which may differ",
                    id.get()
                ),
            }),
            _ => no_such_job(&jid),
        },
        None => json!({
            "jobs": g.iter()
                .filter(|(_, j)| &j.owner == agent_id)
                .map(|((_, id), j)| jobs::snap(*id, j, false))
                .collect::<Vec<_>>(),
            "origin": format!("{agent_id} status — the job table NOW; re-send, it changes"),
        }),
    }
}

/// `clear` — forget this agent's FINISHED rows: one named job, or every one.
///
/// A NAMED job follows the bare form's rule (python `_clear`, audit BV-5): a row
/// still `running` or `stopping` is refused. That row is the only handle `stop`,
/// `cancel`, `delete` and shutdown kill its process through — dropped, the process
/// outlived all four. `clear` is also the one caller that wants the label rather
/// than process liveness — it is deciding what is finished enough to forget.
fn clear_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let want = match job_arg("clear", payload, false) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let mut g = jobs::jobs();
    if let Some(jid) = &want {
        if let Some(j) = jid.find(&g, agent_id) {
            if j.status == "running" || j.status == "stopping" {
                return bad(
                    "clear",
                    "job_id",
                    &format!(
                        "a finished job (job {} is {} — `stop` it, then clear)",
                        py_str(&jid.raw),
                        j.status
                    ),
                );
            }
        }
    }
    let doomed: Vec<jobs::JobKey> = g
        .iter()
        .filter(|((_, id), j)| {
            &j.owner == agent_id
                && want.as_ref().is_none_or(|w| w.id == Some(*id))
                && j.status != "running"
                && j.status != "stopping"
        })
        .map(|(key, _)| key.clone())
        .collect();
    let n = doomed.len();
    for id in doomed {
        // ORDER-PRESERVING: python's dict keeps the surviving rows in start order.
        g.shift_remove(&id);
    }
    json!({ "cleared": n })
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// python `secrets.token_hex(4)` — 4 bytes of OS CSPRNG as 8 hex characters. It
/// names a pasted image's scratch file; a JOB's id is its kernel task id, never
/// this. A failure is FATAL — there is no second source.
pub(crate) fn rand_hex8() -> String {
    let mut buf = [0u8; 4];
    // SAFETY: `buf` is a valid writable buffer of exactly 4 bytes.
    let rc = unsafe { libc::getentropy(buf.as_mut_ptr().cast(), buf.len()) };
    assert!(rc == 0, "getentropy failed: no randomness source");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
