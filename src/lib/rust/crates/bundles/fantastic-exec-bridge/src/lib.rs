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
//! not the record). A verb the running program declared is MAIL for it
//! (`{queued:true}`); jobs are kernel TASKS (`ps`/`cancel`, `on_cancel`
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
//! Declared in [`verbs`]; `reflect` and `docs` render them. The program a job runs may
//! declare verbs of its own (`declare_verbs`): mail is a call of one of those, checked
//! by the same entry against the schema the program declared, and acknowledged
//! `{queued:true}` while a job is live. There is no other mail.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::{io_error_str, py_repr, py_repr_value};
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::refusal::{
    bad, refusal, refusal_hint, with, MISSING_ARG, NOT_FOUND, TRANSPORT_ERROR,
    UNAUTHORIZED, UNHANDLED,
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

// ── the command gate: a REGEX allowlist (py `_allowed_patterns` / `_command_permitted`) ──

/// This leg's `allowed` — regexes a command line must fullmatch to run. Empty ⇒ the
/// leg runs nothing (a legal state: wired but given nothing to run, refusing loudly).
pub fn allowed_patterns(meta: &Map<String, Value>) -> Vec<String> {
    meta.get("allowed")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Compile one `allowed` pattern in PYTHON's `re` dialect.
///
/// `fancy-regex`, not `regex`: python's engine is a backtracker, so an operator's
/// allowlist may legitimately use `(?P<name>…)`, `(?P=name)`, a backreference or
/// lookaround — every one of which `regex` rejects at compile time, turning a
/// working python policy into a leg that permits nothing.
fn compile_pattern(pat: &str) -> Result<fancy_regex::Regex, fancy_regex::Error> {
    fancy_regex::Regex::new(pat)
}

/// Appended to an allowlist refusal when `command` holds several words and `args`
/// is empty — py `COMMAND_ONE_PROGRAM`, the same bytes in every runtime.
pub const COMMAND_ONE_PROGRAM: &str = " — `command` is ONE program and is quoted as one word; put its arguments in `args` (a list), e.g. command=python3 args=[\"-c\", \"print(1)\"]";

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
        // A broken pattern permits nothing; validate_meta refuses new ones. The
        // BARE pattern compiles first: wrapped, an unbalanced one (`x)|(?:.*`)
        // closes the group early and matches every line.
        if compile_pattern(pat).is_err() {
            continue;
        }
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
// `SHELL` and `-l` are unix's; the OS names both (`sys::SHELL_VAR`,
// `sys::LOGIN_ARGS` — `COMSPEC` and nothing on Windows).
//
// It is a LINE, not a pattern: any other argv, and every pipe job, still has to
// fullmatch `allowed`. Who may set `shell` is who may write the record — the
// same authority that writes `allowed`.

/// The child env var a PTY `start` without `command` runs (`SHELL`; the OS
/// names it — [`sys::SHELL_VAR`]).
pub const SHELL_VAR: &str = sys::SHELL_VAR;

/// The argv the record's `shell` permits AFTER `$SHELL`, or `None` when the record
/// permits no shell line. Only JSON `true` and the string `"login"` count — a `1`
/// is not `true` (python compares `True` by identity for the same reason).
pub fn shell_tail(meta: &Map<String, Value>) -> Option<Vec<String>> {
    match meta.get("shell") {
        Some(Value::Bool(true)) => Some(vec![]),
        Some(Value::String(s)) if s == "login" => {
            Some(sys::LOGIN_ARGS.iter().map(|a| (*a).to_string()).collect())
        }
        _ => None,
    }
}

/// One var of the child's environment as the CHILD will see it (the record
/// declares every value a string).
fn child_var(env: &Map<String, Value>, name: &str) -> Option<String> {
    env.get(name).and_then(Value::as_str).map(str::to_string)
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

/// What the bundle is for — its one short man text.
const MAN: &str = "A process edge: runs allowlisted commands as background jobs (pipe or terminal), sealed by default; a job's program may declare verbs of its own and receive them as mail.";

/// The name of a call's verb, for the gate.
fn verb_name(call: &verbs::Call) -> &str {
    use verbs::Call;
    match call {
        Call::Reflect(_) => "reflect",
        Call::Shutdown(_) => "shutdown",
        Call::Start(_) => "start",
        Call::Run(_) => "run",
        Call::Status(_) => "status",
        Call::Stop(_) => "stop",
        Call::Interrupt(_) => "interrupt",
        Call::Clear(_) => "clear",
        Call::Write(_) => "write",
        Call::Resize(_) => "resize",
        Call::Ack(_) => "ack",
        Call::Output(_) => "output",
        Call::Tail(_) => "tail",
        Call::Grep(_) => "grep",
        Call::PasteImage(_) => "paste_image",
        Call::Connector(_) => "connector",
        Call::Monitor(_) => "monitor",
        Call::Monitors(_) => "monitors",
        Call::Unmonitor(_) => "unmonitor",
        Call::DeclareVerbs(_) => "declare_verbs",
        Call::Dynamic(c) => &c.name,
    }
}

#[async_trait]
impl Handler for ExecBridgeBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        "exec_bridge"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    /// The verbs the agent's running program declared (mail).
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        mail::declared(&agent.id)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    /// Refuse a write this bundle cannot honour, at the WRITE, next to the typo
    /// (py-canonical `validate_meta`): what the declared record cannot say, `allowed`
    /// as compilable regexes.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        // `allowed` must hold compilable regexes: what a declaration cannot say.
        for pat in allowed_patterns(meta) {
            if let Err(e) = compile_pattern(&pat) {
                return Some(format!(
                    "allowed contains a broken regex {}: {e}",
                    py_repr(&pat)
                ));
            }
        }
        None
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(self.handle_binary(agent_id, call, None, kernel).await?.0)
    }

    /// The kernel's dispatch, every verb: `write` reads ITS key — the request body at
    /// `bytes`, written verbatim — and `paste_image` the body at `data`. A body under
    /// any other key, or none, is not the chunk.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        use verbs::Call;
        // AUTH GATE — the process edge, gated like any io_bridge leg (SEALED by
        // default). `reflect` and `connector` are UNGATED: discovery must work on a
        // sealed bridge. Every other verb is gated, MAIL INCLUDED (python `handler`):
        // a sealed leg delivers nothing to its program — the relay's inbox drain
        // refuses the same mail, since the substrate's fanout reaches it before this
        // handler runs.
        if !matches!(call, Call::Reflect(_) | Call::Connector(_)) {
            let meta = snapshot_meta(agent_id, kernel);
            if let Some(denied) = gate(&meta, agent_id, verb_name(&call)) {
                // The fanout has queued a copy of this mail for the program already:
                // note the refusal now so the drain drops it whatever the record
                // says when it dequeues.
                if let Call::Dynamic(c) = &call {
                    if relay::has_owner_inbox(agent_id) {
                        relay::note_refused(agent_id, &mail::payload_of(c));
                    }
                }
                return Ok((Some(denied), None));
            }
        }
        let reply = match call {
            Call::Reflect(_) => reflect_reply(agent_id, kernel),
            Call::Shutdown(_) => Value::Null,
            Call::Start(a) => {
                start_reply(agent_id, a.command.as_deref(), a.args, a.pty, kernel).await
            }
            Call::Run(a) => run_reply(agent_id, a, kernel).await,
            Call::Status(a) => status_reply(agent_id, &a).await,
            Call::Clear(a) => clear_reply(agent_id, a.job_id.map(JobRef::of).as_ref()),
            Call::Interrupt(a) => interrupt_reply(agent_id, a.job_id.map(JobRef::of).as_ref()),
            Call::Stop(a) => stop_reply(agent_id, a.job_id.map(JobRef::of).as_ref()),
            Call::Write(a) => pty::write_reply(agent_id, &a, body_at(body.as_ref(), "bytes")).await,
            Call::Resize(a) => pty::resize_reply(agent_id, &a),
            Call::Ack(a) => pty::ack_reply(agent_id, &a),
            Call::Output(a) => pty::output_reply(agent_id, &a),
            Call::Tail(a) => tail::tail_reply(agent_id, &JobRef::of(a.job_id), a.lines, a.stream),
            Call::Grep(a) => tail::grep_reply(agent_id, &a),
            Call::Monitor(a) => monitor::monitor_reply(agent_id, &a, kernel).await,
            Call::Monitors(_) => monitor::monitors_reply(agent_id),
            Call::Unmonitor(a) => monitor::unmonitor_reply(agent_id, a.monitor_id, kernel).await,
            // `data` is the request body: on the text channel there is none to paste.
            Call::PasteImage(a) => match body_at(body.as_ref(), "data") {
                Some(data) => pty::paste_image_reply(agent_id, &a, data).await,
                None => pty::paste_image_text(agent_id, &a),
            },
            // The reference wire client SOURCE — a model pastes it at the top of
            // its program instead of re-deriving the socket framing (int(fd),
            // buffering, not-seekable). Ungated like reflect.
            Call::Connector(_) => json!({"source": CONNECTOR_SRC}),
            Call::DeclareVerbs(a) => mail::declare_reply(agent_id, &a),
            // MAIL — a verb the running program declared, its arguments already
            // checked against the declared schema. The substrate's fanout has ALREADY
            // delivered it to the job's `_xbself_` proxy, so this only ACKs.
            // Re-emitting would double it.
            Call::Dynamic(c) => {
                if jobs::has_running_job(agent_id) {
                    json!({"queued": true})
                } else {
                    let mut r = refusal(
                        UNHANDLED,
                        format!(
                            "exec_bridge: {} is mail and no job is running to receive it",
                            py_repr(&c.name)
                        ),
                    );
                    with(&mut r, "verb", json!(c.name));
                    r
                }
            }
        };
        Ok((Some(reply), None))
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
        mail::forget(agent_id);
        Ok(())
    }
    /// Shutdown frees what delete frees — declared, never a fallback.
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
    /// cancelling one must not take out the other.
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
/// absent `ingress_rule` resolves to deny-inbound, and every denial
/// teaches the fix. `kind="spawn"` — that is what is being authorized; no rule
/// inspects the kind, so a new one costs nothing and the audit trail is honest.
///
/// The reply FORWARDS the rule's own `Denial` — reason, hint, see. A hint
/// hand-rolled here would teach a different repair per bundle and per runtime;
/// the rule that refused is the thing that knows the repair.
fn gate(meta: &Map<String, Value>, agent_id: &AgentId, verb: &str) -> Option<Value> {
    let rules = fantastic_io_bridge::authorizer::Rules::of(meta).unwrap_or_default();
    let rule = fantastic_io_bridge::authorizer::ingress::resolve(&rules);
    let action = fantastic_io_bridge::authorizer::Action {
        kind: "spawn",
        target: agent_id.0.as_str(),
        verb,
        // NO CREDENTIAL. A token is a WIRE-ENVELOPE fact and the process
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
        "sentence": MAN,
        "cwd": cwd_val,
        // THE GATE, advertised: a caller must see what it may run, where, and with
        // which of the kernel's env vars.
        "allowed": allowed_patterns(&meta),
        "inherit_env": meta.get("inherit_env").cloned().unwrap_or_else(|| json!([])),
        // The shell line the record permits without an `allowed` entry: false
        // (none), true ($SHELL bare) or "login" ($SHELL -l).
        "shell": meta.get("shell").cloned().unwrap_or(json!(false)),
        "running": running,
    });
    // The auth POSTURE, python's `describe(rec)` — `**_describe(rec)` is the last
    // thing python's reflect spreads, so these keys land last and in its order.
    if let (Some(o), Some(posture)) = (
        out.as_object_mut(),
        fantastic_io_bridge::authorizer::Rules::of(&meta)
            .unwrap_or_default()
            .describe()
            .as_object(),
    ) {
        for (k, v) in posture {
            o.insert(k.clone(), v.clone());
        }
    }
    out
}

pub mod jobs;
pub mod mail;
mod monitor;
pub mod pty;
pub mod record;
pub mod relay;
pub mod spawn;
mod sys;
mod tail;
pub mod verbs;

#[cfg(test)]
mod pty_tests;
#[cfg(test)]
mod shell_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_monitor;

/// What `interrupt` / `stop` do to a live job.
#[derive(Clone, Copy)]
enum Signal {
    /// Its Ctrl-C ([`jobs::interrupt`]).
    Interrupt,
    /// Its whole session, killed ([`jobs::kill`]).
    Kill,
}

/// `interrupt` / `stop` — signal this agent's jobs, or one named job.
///
/// Both now address the JOB TABLE. There is exactly one registry: a private pid set
/// alongside it would be a second truth, and `start`'s jobs would be invisible to it.
fn signal_jobs(agent_id: &AgentId, want: Option<&JobRef>, sig: Signal) -> u64 {
    let mut g = jobs::jobs();
    let mut n = 0u64;
    for ((_, id), j) in g.iter_mut() {
        if &j.owner != agent_id || want.is_some_and(|w| w.id != *id) {
            continue;
        }
        if !jobs::alive(j) {
            continue; // already gone — never pretend
        }
        let signalled = match sig {
            Signal::Kill => {
                j.status = "stopping".to_string();
                jobs::kill(j)
            }
            Signal::Interrupt => jobs::interrupt(j),
        };
        if signalled {
            n += 1;
        }
    }
    n
}

/// The `not_found` refusal for a named job this agent does not own, or `None`.
///
/// A `job_id` naming nothing must not answer `{killed:0}`/`{interrupted:0}` —
/// the same reply as "it had already exited", so a typo would read as success —
/// nor a bare `{error}` with no code: a caller branches on
/// `reason == "not_found"` (what the corpus and the e2e scenarios pin).
fn no_job(agent_id: &AgentId, want: Option<&JobRef>) -> Option<Value> {
    let jid = want?;
    match jid.find(&jobs::jobs(), agent_id) {
        None => Some(no_such_job(jid)),
        Some(_) => None,
    }
}

/// A job named by a caller: the kernel task id it names.
pub(crate) struct JobRef {
    /// The kernel task id it names.
    pub(crate) id: TaskId,
}

impl JobRef {
    /// The job a call named by its task id.
    pub(crate) fn of(id: i64) -> Self {
        Self { id: TaskId(id) }
    }

    /// This agent's row for the named job, when there is one. A job of ANOTHER
    /// agent is not found, exactly like a job that never existed.
    pub(crate) fn find<'a>(
        &self,
        g: &'a indexmap::IndexMap<jobs::JobKey, jobs::Job>,
        owner: &AgentId,
    ) -> Option<&'a jobs::Job> {
        g.get(&(owner.clone(), self.id))
    }
}

/// The `not_found` refusal naming `jid` as it was sent (python
/// `refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)`).
pub(crate) fn no_such_job(jid: &JobRef) -> Value {
    let mut v = refusal(
        NOT_FOUND,
        format!("exec_bridge: no job {}", jid.id.0),
    );
    with(&mut v, "job_id", json!(jid.id.0));
    v
}

fn interrupt_reply(agent_id: &AgentId, want: Option<&JobRef>) -> Value {
    no_job(agent_id, want).unwrap_or_else(
        || json!({"interrupted": signal_jobs(agent_id, want, Signal::Interrupt)}),
    )
}

fn stop_reply(agent_id: &AgentId, want: Option<&JobRef>) -> Value {
    no_job(agent_id, want)
        .unwrap_or_else(|| json!({"killed": signal_jobs(agent_id, want, Signal::Kill)}))
}

/// The agent's own folder (empty when it is gone).
fn agent_own_root(kernel: &Kernel, id: &AgentId) -> std::path::PathBuf {
    kernel
        .agents
        .get(id)
        .map(|a| a.root_path.clone())
        .unwrap_or_default()
}

/// A `cwd` the kernel's rule cannot place, as a refusal naming it.
fn cwd_refusal(cwd: &str, why: &str) -> Value {
    let mut v = refusal_hint(
        MISSING_ARG,
        format!("exec_bridge: cwd {cwd:?}: {why}"),
        "set `cwd` to a folder under the work folder, or @state/…",
    );
    with(&mut v, "arg", json!("cwd"));
    v
}

/// `start` as the answer a caller reads: the job started, or the refusal.
async fn start_reply(
    agent_id: &AgentId,
    command: Option<&str>,
    args: Option<Vec<String>>,
    pty: Option<verbs::Terminal>,
    kernel: &Arc<Kernel>,
) -> Value {
    match start_job(agent_id, command, args, pty, kernel).await {
        Ok((job_id, is_pty)) => {
            json!({"job_id": job_id.get(), "status": "running", "pty": is_pty})
        }
        Err(refused) => refused,
    }
}

/// Start the job: its task id and whether it runs on a terminal, or the refusal.
async fn start_job(
    agent_id: &AgentId,
    command: Option<&str>,
    args: Option<Vec<String>>,
    pty: Option<verbs::Terminal>,
    kernel: &Arc<Kernel>,
) -> Result<(TaskId, bool), Value> {
    let meta = snapshot_meta(agent_id, kernel);
    // EVERY refusal on this verb carries a CODE, so nothing has to tell a typo
    // from a policy refusal by matching prose.
    // WHERE and WITH WHAT a command runs are the OPERATOR's: the call declares no
    // `env` or `cwd` (they would change what an allowlisted line actually runs), nor
    // a `notify`: a WAKE-UP is a monitor's, never the start's.
    let mut args: Vec<String> = args.unwrap_or_default();
    let dims = pty.map(|t| (t.cols, t.rows));
    // The child's WHOLE environment, from the record — built before the command
    // is resolved, because an absent `command` on a PTY start is read from it.
    let mut env = child_env(&meta);
    let command = match command {
        // AUTODISCOVERY, not a default: the shell is whatever the record passed
        // down to the child as SHELL. No chain (zsh → bash → sh) behind it — a
        // child env without one is refused, naming where it comes from.
        None if dims.is_some() => match child_var(&env, SHELL_VAR).filter(|s| !s.is_empty()) {
            Some(shell) => {
                // …and with no `args` either, it is the SHELL LINE the record
                // permits: `$SHELL` under `shell: true`, its login line under
                // `shell: "login"` (`-l`; nothing on Windows) — a client asks for
                // "the shell" without knowing which OS, or which flag, makes it a
                // login shell (py `raw_args = _shell_tail(rec) or []`).
                if args.is_empty() {
                    args = shell_tail(&meta).unwrap_or_default();
                }
                shell
            }
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
                return Err(v);
            }
        },
        None => String::new(),
        Some(v) => v.to_string(),
    };
    // A given `command` is never blank (the declared `pattern`) and a call naming
    // neither `command` nor `pty` is refused by the engine (the declared `anyOf`).
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
        if args.is_empty() && command.split_whitespace().count() > 1 {
            // A whole line in `command` is quoted as ONE word, so no pattern over
            // `<program> <args>` can ever match it — say where the words go.
            why.push_str(COMMAND_ONE_PROGRAM);
        }
        return Err(refusal_hint(
            UNAUTHORIZED,
            why,
            "the leg's `allowed` policy decides; an operator may widen it",
        ));
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
        return Err(v);
    };
    // The kernel's one record-path rule: `@state`/`@self` under their folders,
    // anything else (relative or absolute) under the work folder.
    // An absolute path needs no base: it is its own place, so the answer is the
    // same with or without a folder to join it to.
    let cwd = if std::path::Path::new(&cwd).is_absolute() {
        cwd
    } else {
        match kernel.record_root(&cwd, &agent_own_root(kernel, agent_id)) {
            Ok((base, rel)) => match std::path::absolute(base.join(rel)) {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(e) => return Err(cwd_refusal(&cwd, &e.to_string())),
            },
            Err(e) => return Err(cwd_refusal(&cwd, &e)),
        }
    };

    let spawned = match dims {
        None => crate::spawn::spawn_job(&argv, agent_id.as_str(), &env, &cwd).map(|s| (s, None)),
        Some((cols, rows)) => {
            env.insert("TERM".to_string(), json!("xterm-256color"));
            crate::spawn::spawn_pty(&argv, agent_id.as_str(), &env, &cwd, cols, rows)
                .map(|(s, master)| (s, Some(pty::Pty::new(master, cols, rows))))
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
            return Err(v);
        }
        Err(e) => {
            // py `f"exec_bridge: spawn failed: {e}"` over the OSError
            // `create_subprocess_exec` raises — `[Errno 2] No such file or
            // directory: 'nope'`, the COMMAND as the filename. A MISSING `cwd` is
            // named instead, as CPython names it (its `noexec:chdir` path): a leg
            // whose cwd does not exist answering `No such file or directory:
            // 'python3'` would send the model hunting an interpreter that is there.
            let culprit = if sys::cwd_failed(&e, &cwd) {
                &cwd
            } else {
                &command
            };
            return Err(refusal(
                TRANSPORT_ERROR,
                format!(
                    "exec_bridge: spawn failed: {}",
                    io_error_str(&e, std::path::Path::new(culprit))
                ),
            ));
        }
    };
    let crate::spawn::Spawned { child, ctrl } = spawned;
    let Some(pid) = child.id() else {
        return Err(refusal(
            TRANSPORT_ERROR,
            "exec_bridge: child exited before it could be tracked",
        ));
    };
    let (rd, wr) = ctrl.into_split();
    // THE PROGRAM'S SENDS ARE THIS AGENT'S SENDS — `spawn_task` carries the
    // dispatch identity into the relay. A `tokio::spawn` inherits no task-local,
    // so the relay would forward frames with NO sender, i.e. as the OPERATOR,
    // ungated, and a spawned program could reach any agent. The runner's
    // `can_call` bounds its programs.
    // The relay's stop signal, held on the job row. `teardown` raises it instead of
    // aborting this task, so the relay leaves by its own read loop and the proxy
    // release after that loop actually runs.
    let stop = Arc::new(tokio::sync::Notify::new());

    // THE PUMP IS THE JOB'S TASK, and the job's id IS that task's id — minted by
    // the kernel's one counter when the pump is forked (python `_start`). One number
    // names the job everywhere: `status`/`stop`/`write` take it as `job_id`, `ps`
    // lists it, `cancel task_id=` kills it. (A random hex id would not survive a
    // model's `<job_id>04203581</job_id>`: coerced into an int or an infinite
    // float, it could not be addressed again.)
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
            line_ring: std::collections::VecDeque::new(),
            line_ring_bytes: 0,
            line_ring_dropped: 0,
            pty: pty_state.clone(),
            monitors_closed: false,
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
    Ok((job_id, pty_state.is_some()))
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
    // What the OS itself needs in every process (`sys::OS_ENV`: nothing on unix,
    // `SYSTEMROOT` on Windows) comes down from the kernel's env even when
    // `inherit_env` leaves it out; the record's `env` still has the last word.
    for n in sys::OS_ENV {
        if !env.contains_key(*n) {
            if let Ok(v) = std::env::var(n) {
                env.insert((*n).to_string(), json!(v));
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

/// `status` — one job (full output) or all of THIS agent's jobs.
///
/// `wait_s` (0–120, with a `job_id`) waits for the job to stop RUNNING before
/// answering, so a caller does not have to poll.
async fn status_reply(agent_id: &AgentId, a: &verbs::Status) -> Value {
    let want = a.job_id.map(JobRef::of);
    if let Some(secs) = a.wait_s {
        let Some(jid) = &want else {
            return bad("status", "wait_s", "a job_id to wait for");
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
        loop {
            let live = {
                let g = jobs::jobs();
                match jid.find(&g, agent_id) {
                    Some(j) => j.status == "running" || j.status == "stopping",
                    None => false,
                }
            };
            if !live || std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    let g = jobs::jobs();
    match want {
        Some(jid) => match jid.find(&g, agent_id) {
            Some(j) => json!({
                "jobs": [jobs::snap(jid.id, j, true)],
                "origin": format!(
                    "{agent_id} status {{job_id: {}}} — a live job; re-send for its \
                     output NOW, which may differ",
                    jid.id.get()
                ),
            }),
            None => no_such_job(&jid),
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

/// `run` — start, wait, and answer with the output in ONE reply.
///
/// `wait_s` (default 60, 0–600) bounds the wait exactly like `status`'s: the job
/// is polled every 200 ms and the reply comes as soon as it is no longer
/// `running`/`stopping`. `tail` (default 200, 1–500) is how many of its last
/// lines come back. A job still running at the deadline is returned `running`
/// with its `job_id`, so the caller continues with `status wait_s` / `tail`.
async fn run_reply(agent_id: &AgentId, a: verbs::Run, kernel: &Arc<Kernel>) -> Value {
    let (job_id, _) =
        match start_job(agent_id, a.command.as_deref(), a.args, a.pty, kernel).await {
            Ok(started) => started,
            // A refusal from `start` (no command, not allowed, no cwd) is the answer.
            Err(refused) => return refused,
        };
    let jid = JobRef::of(job_id.get());
    let wait_s = a.wait_s;
    let tail = a.tail;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(wait_s);
    loop {
        let live = {
            let g = jobs::jobs();
            match jid.find(&g, agent_id) {
                Some(j) => j.status == "running" || j.status == "stopping",
                None => false,
            }
        };
        if !live || std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let (status, exit_code) = {
        let g = jobs::jobs();
        match jid.find(&g, agent_id) {
            Some(j) => (j.status.clone(), j.exit_code),
            None => ("running".to_string(), None),
        }
    };
    let mut reply = tail::tail_reply(agent_id, &jid, tail, verbs::Stream::Both);
    with(&mut reply, "status", json!(status));
    with(&mut reply, "exit_code", json!(exit_code));
    reply
}

/// `clear` — forget this agent's FINISHED rows: one named job, or every one.
///
/// A NAMED job follows the bare form's rule (python `_clear`): a row
/// still `running` or `stopping` is refused. That row is the only handle `stop`,
/// `cancel`, `delete` and shutdown kill its process through — dropped, the process
/// would outlive all four. `clear` is also the one caller that wants the label rather
/// than process liveness — it is deciding what is finished enough to forget.
fn clear_reply(agent_id: &AgentId, want: Option<&JobRef>) -> Value {
    let mut g = jobs::jobs();
    if let Some(jid) = want {
        if let Some(j) = jid.find(&g, agent_id) {
            if j.status == "running" || j.status == "stopping" {
                return bad(
                    "clear",
                    "job_id",
                    &format!(
                        "a finished job (job {} is {} — `stop` it, then clear)",
                        jid.id.0,
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
                && want.is_none_or(|w| w.id == *id)
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
    fantastic_kernel::lifecycle::fill_random(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
