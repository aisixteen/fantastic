//! Process edge of the io family — rust mirror of the canonical python
//! `io/exec_bridge` (which is canonical; on disagreement this crate is wrong).
//!
//! Crossing kernel↔subprocess is gated like any io_bridge leg: **SEALED BY
//! DEFAULT** (absent `ingress_rule` ⇒ every verb except `reflect` denies
//! `{reason:"unauthorized"}`), and bound by a **regex command ALLOWLIST**:
//! `allowed` on the record is a list of regexes, and `start {command, args}`
//! refuses unless the shell-joined line fullmatches one. No per-call override,
//! no default, no PATH fallback — an empty `allowed` runs nothing. The withdrawn
//! `code`/`shim`/`interpreter`/`interpreters`/`argv_template` fields are refused
//! at WRITE time (`validate_meta`) and at `start`.
//!
//! It is a GENERAL-PURPOSE COMMAND runner, **not** a code runner. A program is a
//! FILE some process (or a `write_stream`) put on disk; running it is `command` +
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
//! | `start`    | `command:str` (req), `args:list[str]?`, `env:dict?`, `cwd?`     | `{job_id, status:"running"}` at once           |
//! | `status`   | `job_id:str?`                                                   | one job with its output, or all of this agent's |
//! | `clear`    | `job_id:str?`                                                   | drops finished rows of this agent              |
//! | `interrupt`| `job_id:str?`                                                   | `{interrupted: int}` (SIGINT to in-flight)     |
//! | `stop`     | `job_id:str?`                                                   | `{killed: int}` (SIGKILL to in-flight)         |
//! | `boot`     | _none_                                                          | no-op → `null`                                 |
//!
//! Anything else is MAIL for the running program: `{queued:true}` when a job is
//! live, a loud error naming the verb when none is.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::{io_error_str, py_repr, py_repr_value, py_str, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{
    bad, missing, refusal, refusal_hint, with, NOT_FOUND, TRANSPORT_ERROR, UNAUTHORIZED,
};
use fantastic_kernel::{AgentId, Kernel, TaskId};
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

// ── bundle impl ─────────────────────────────────────────────────────

/// The exec_bridge bundle.
pub struct ExecBridgeBundle;

#[async_trait]
impl Bundle for ExecBridgeBundle {
    fn name(&self) -> &str {
        "exec_bridge"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// Refuse a write this bundle cannot honour, at the WRITE, next to the typo
    /// (py-canonical `validate_meta`). Rejects the withdrawn code-runner fields and
    /// validates that `allowed` is a non-empty list of compilable regexes.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        // ONLY an UNKNOWN RULE NAME is refused (py `resolve_ingress(...).unknown`):
        // a falsy spec and a type-less object both seal silently, and
        // `password_ingress` resolves fine here — it is the stem check below that
        // refuses it on this edge.
        if let Some(spec) = meta.get("ingress_rule") {
            if fantastic_io_bridge::authorizer::ingress::unknown(Some(spec)).is_some() {
                return Some(format!(
                    "ingress_rule={} is not a rule this bundle implements — \
                     legal values: allow_all_ingress, deny_ingress",
                    py_repr_value(spec)
                ));
            }
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
        // The withdrawn code-runner fields are refused at WRITE time. This is a
        // GENERAL-PURPOSE command runner; gate it with `allowed`.
        for gone in [
            "code",
            "shim",
            "interpreter",
            "interpreters",
            "argv_template",
        ] {
            if meta.get(gone).is_some_and(|v| !v.is_null()) {
                return Some(format!(
                    "exec_bridge: `{gone}` is not a field — this is a general-purpose \
                     COMMAND runner; gate it with `allowed=[<regex>, ...]`"
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
        // sealed bridge. Mail (the unknown-verb branch below) is deliberately not
        // gated, matching py: the substrate's fanout already delivered the payload;
        // this only ACKs.
        //
        // `shutdown` is NOT in this set and NOT a verb (A4.3): python's VERBS has no
        // such key, so it is MAIL like any other unowned type.
        let owned = matches!(
            verb,
            "start" | "boot" | "status" | "clear" | "interrupt" | "stop"
        );
        if owned {
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
            // The reference wire client SOURCE — a model pastes it at the top of
            // its program instead of re-deriving the socket framing (measured
            // 2026-09-01, E30 subai:rust: ten turns lost to int(fd)/buffering/
            // not-seekable before the first working frame). Ungated like reflect.
            "connector" => json!({
                "source": CONNECTOR_SRC,
                "howto": "Write `source` verbatim as the first lines of your program file, then \
                          your code below it. It opens FANTASTIC_CTRL_FD (a socket, length-prefixed \
                          frames; a payload carrying a bytes value rides the binary channel raw: \
                          `write_stream` puts its chunk in `bytes`, `read_stream` passes `bytes: b''` \
                          and gets its chunk back raw in the reply's `bytes`) and \
                          exposes `kernel.send(target, payload) -> reply` (raises if the relay could \
                          not deliver), \
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
                    json!({"error": format!(
                        "exec_bridge: unknown type {} (no running job to receive it)",
                        py_repr_value(raw_type)
                    )})
                }
            }
        };
        Ok(Some(reply))
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
        for j in g
            .values_mut()
            .filter(|j| &j.owner == agent_id && &j.task == task_id)
        {
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
    let rule = match fantastic_io_bridge::authorizer::ingress::resolve(spec.as_ref()) {
        Ok(r) => r,
        Err(e) => return Some(json!({"error": e})),
    };
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
    // py `rec.get("cwd") or "<process default>"`: TRUTHINESS over the raw value, so
    // a non-string cwd is reported as written rather than silently dropped.
    let cwd_val = match meta.get("cwd").filter(|v| py_truthy(v)) {
        Some(v) => v.clone(),
        None => json!("<process default>"),
    };
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
        // THE GATE, advertised: a caller must see what it may run.
        "allowed": allowed_patterns(&meta),
        "running": running,
        // py `{n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()}`
        // — the FIRST LINE of each verb's docstring, in `VERBS` order. Several are
        // truncated mid-sentence; that is the python text, not an abbreviation.
        "verbs": {
            "start": "args: command:str (req), args:list[str]? (argv, default []), env:dict? (extra",
            "status": "args: job_id:str? — one job (with full stdout/stderr), or all of this agent's",
            "stop": "args: job_id:str? — SIGKILL one job or ALL of this agent's running jobs.",
            "interrupt": "args: job_id:str? — SIGINT one job or ALL of this agent's running jobs.",
            "clear": "args: job_id:str? — drop finished jobs (one, or all non-running) from the RAM",
            "connector": "no args — the reference wire client SOURCE (python): paste `source` verbatim at the top of your program and it gets a `kernel` global (send/emit/watch/on_message); returns {source, howto}.",
            "reflect": "Identity + running job count + auth posture. No args.",
            "boot": "No-op. Nothing auto-restarts: a command is started by a caller, not by the",
        },
        "emits": {
            "job_started": "{type:'job_started', job_id}",
            "progress": "{type:'progress', job_id, n, line, stream} — one per output line",
            "job_done": "{type:'job_done', job_id, exit_code, ok, stdout, stderr}",
        },
        "wire": "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD, a BIDIRECTIONAL socket (one end of a socketpair): frame is [4-byte BE length | kind | rest], both directions — kind T: a JSON envelope; kind B: [4-byte BE H | H-byte JSON header | raw body], the header's `_binary_path` naming where the body belongs (a stream chunk rides raw, never base64). To CALL another agent: write {op:'send', rid:<your id>, target, payload} and the reply COMES BACK on the SAME fd as {op:'reply', rid:<same id>, data} — the fd is not one-way. It is not inline, though: read frames in a loop and skip any whose rid isn't yours (event/inbox frames can arrive first) until you see your reply. ops: send/emit/reflect/watch/on_message. A command that never reads the fd runs exactly like any OS process.",
        "inbox": "Any verb NOT listed in `verbs` is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is an error — a script that only defines something exits before its first turn arrives.",
        "tasks": "Every running job holds a row in the kernel TASK TABLE (`task_id` in each `status` snapshot; listed by the universal `ps`). `cancel task_id=<id>` kills it — the OS subprocess is SIGKILLed by this bundle's `on_cancel` hook before the kernel aborts the task, so nothing is orphaned.",
        "authoring": "A general-purpose command runner: <type>start</type> <command>…</command> <args><item>…</item></args>, gated by the `allowed` regexes on the full command line.",
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
pub mod relay;
pub mod spawn;

#[cfg(test)]
mod tests;

/// `interrupt` / `stop` — signal this agent's jobs, or one named job.
///
/// Both now address the JOB TABLE. There is exactly one registry: a private pid set
/// alongside it would be a second truth, and `start`'s jobs would be invisible to it.
fn signal_jobs(agent_id: &AgentId, payload: &Value, sig: nix::sys::signal::Signal) -> u64 {
    let want = job_id(payload).and_then(Value::as_str);
    let mut g = jobs::jobs();
    let mut n = 0u64;
    for (id, j) in g.iter_mut() {
        if &j.owner != agent_id || want.is_some_and(|w| w != id.as_str()) {
            continue;
        }
        if !jobs::alive(j) {
            continue; // already gone — never pretend
        }
        if sig == nix::sys::signal::Signal::SIGKILL {
            j.status = "stopping".to_string();
        }
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(j.pid as i32), sig).is_ok() {
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
fn no_job(agent_id: &AgentId, payload: &Value) -> Option<Value> {
    let jid = job_id(payload)?;
    // python indexes `_jobs` with the RAW value: only a string can ever name a row,
    // so a truthy non-string is simply not found — and the refusal echoes it as sent.
    let known = jid
        .as_str()
        .is_some_and(|s| jobs::jobs().get(s).is_some_and(|j| &j.owner == agent_id));
    if known {
        return None;
    }
    let mut v = refusal(
        NOT_FOUND,
        format!("exec_bridge: no job {}", py_repr_value(jid)),
    );
    with(&mut v, "job_id", jid.clone());
    Some(v)
}

/// `payload["job_id"]` under PYTHON TRUTHINESS (A4.8).
///
/// `None` = every job of this agent — python writes `if jid:`, so an absent key,
/// `null`, `""`, `0`, `[]` and `{}` all mean ALL, and only a truthy value names one
/// job. The raw `Value` is returned because every refusal echoes it verbatim.
fn job_id(payload: &Value) -> Option<&Value> {
    payload.get("job_id").filter(|v| py_truthy(v))
}

fn interrupt_reply(agent_id: &AgentId, payload: &Value) -> Value {
    no_job(agent_id, payload).unwrap_or_else(
        || json!({"interrupted": signal_jobs(agent_id, payload, nix::sys::signal::Signal::SIGINT)}),
    )
}

fn stop_reply(agent_id: &AgentId, payload: &Value) -> Value {
    no_job(agent_id, payload).unwrap_or_else(
        || json!({"killed": signal_jobs(agent_id, payload, nix::sys::signal::Signal::SIGKILL)}),
    )
}

/// `start` — spawn the COMMAND in the BACKGROUND and return at once.
///
/// args: `command:str` (req), `args:list[str]?` (argv, default `[]`), `env:dict?`
/// (extra env vars), `cwd:str?`. The FULL line (command + args, shell-joined) must
/// fullmatch one of the record's `allowed` regexes or the start is refused naming
/// the patterns. A `code` field is refused explicitly — this runs COMMANDS, not code.
async fn start_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    // EVERY refusal on this verb carries a CODE. Sibling argument errors used to
    // split — `args`/`env` answered `bad_arg` while `command`/`code`/the policy
    // wall answered a bare `{error}` — so nothing could tell a typo from a policy
    // refusal without matching prose.
    if payload.get("code").is_some_and(|v| !v.is_null()) {
        return bad("start", "code", "absent — run a command instead");
    }
    // py `not isinstance(command, str) or not command.strip()`: a string of nothing
    // but WHITESPACE is missing, and `str.strip()` strips every python space
    // character, not just ASCII.
    let command = match payload.get("command").and_then(Value::as_str) {
        Some(c) if c.chars().any(|ch| !fantastic_bundle::is_space(ch)) => c.to_string(),
        _ => return missing("start", "command"),
    };
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
    // The FULL argv, shell-joined, matched against the allowlist BEFORE spawning.
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(command.clone());
    argv.extend(args.iter().cloned());
    let line = shlex_join(&argv);
    if let Some(why) = command_permitted(&meta, &line) {
        // An AUTHORIZATION denial, not a bad argument: the command line is
        // well-formed and the LEG refused it. Same code the ingress gate answers,
        // so one branch covers "the operator has not opened this".
        return refusal_hint(
            UNAUTHORIZED,
            why,
            "the leg's `allowed` policy decides; an operator may widen it",
        );
    }
    // The per-call `env` block — py `payload.get("env") or {}`, so ANY falsy value
    // (absent, null, `{}`, `0`, `""`, `[]`) is an empty block and only a truthy
    // non-dict refuses. Values are str/int/float/bool, stringified by `str()`.
    let env_extra: Map<String, Value> = match payload.get("env").filter(|v| py_truthy(v)) {
        None => Map::new(),
        Some(Value::Object(m)) => {
            for v in m.values() {
                if !matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
                    return bad("start", "env", "a dict of str -> str");
                }
            }
            m.clone()
        }
        Some(_) => return bad("start", "env", "a dict of str -> str"),
    };
    // py `payload.get("cwd") or rec.get("cwd") or None` — truthiness, and the raw
    // value is what the job row reports; `proc.spawn_argv` then uses it only when it
    // is a non-empty STRING.
    let cwd = payload
        .get("cwd")
        .filter(|v| py_truthy(v))
        .or_else(|| meta.get("cwd").filter(|v| py_truthy(v)))
        .cloned()
        .unwrap_or(Value::Null);

    let spawned = match crate::spawn::spawn_job(&argv, agent_id.as_str(), &env_extra, cwd.as_str())
    {
        Ok(s) => s,
        Err(e) => {
            // py `f"exec_bridge: spawn failed: {e}"` over the OSError
            // `create_subprocess_exec` raises — `[Errno 2] No such file or
            // directory: 'nope'`, the COMMAND as the filename.
            return refusal(
                TRANSPORT_ERROR,
                format!(
                    "exec_bridge: spawn failed: {}",
                    io_error_str(&e, std::path::Path::new(&command))
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
    let job_id = format!("{:08x}", rand_job_id());

    let (rd, wr) = ctrl.into_split();
    // THE PROGRAM'S SENDS ARE THIS AGENT'S SENDS — carried by `spawn_task` now,
    // not re-entered by hand here. A `tokio::spawn` inherits no task-local, so
    // the relay used to forward frames with NO sender, i.e. as the OPERATOR,
    // ungated (measured live 2026-09-01, E30 subai:rust: a spawned program "is
    // privileged as the root", reaching any agent). The runner's `can_call`
    // bounds its programs.
    // The relay's stop signal, held on the job row. `teardown` raises it instead of
    // aborting this task, so the relay leaves by its own read loop and the proxy
    // release after that loop actually runs.
    let stop = Arc::new(tokio::sync::Notify::new());
    // REGISTERED: the relay outlives this `start` dispatch and issues a send per
    // frame the program writes, so it belongs in `ps` and within reach of
    // `cancel` like the pump below.
    let ctrl_task = kernel.spawn_task(
        agent_id,
        "relay",
        crate::relay::serve(
            rd,
            wr,
            agent_id.clone(),
            job_id.clone(),
            Arc::clone(kernel),
            Arc::clone(&stop),
        ),
    );

    // spawn_task + row insert in ONE critical section, so no window exists in which a
    // live pid has no row. The task body takes its inputs BY VALUE — reading our own
    // row from inside the pump would race this very lock.
    let owner = agent_id.clone();
    let jid = job_id.clone();
    let k = Arc::clone(kernel);
    let task = kernel.spawn_task(agent_id, "start", async move {
        jobs::pump(owner, jid, child, k).await;
    });
    jobs::jobs().insert(
        job_id.clone(),
        jobs::Job {
            owner: agent_id.clone(),
            pid,
            task,
            ctrl: ctrl_task,
            stop,
            // WHAT ran — so a caller can re-issue this job verbatim.
            command,
            args,
            cwd,
            status: "running".to_string(),
            started_at: now_secs(),
            exit_code: None,
            lines: 0,
            last_line: String::new(),
            stdout: String::new(),
            stderr: String::new(),
        },
    );
    kernel
        .emit(agent_id, json!({"type": "job_started", "job_id": job_id}))
        .await;
    json!({"job_id": job_id, "status": "running"})
}

/// `boot` — a NO-OP returning null. Nothing auto-restarts: a command is started by a
/// caller, not by the record (py `_boot`).
async fn boot_reply(_agent_id: &AgentId, _kernel: &Arc<Kernel>) -> Value {
    Value::Null
}

/// `status` — one job (full output) or all of THIS agent's jobs.
fn status_reply(agent_id: &AgentId, payload: &Value) -> Value {
    if let Some(gone) = no_job(agent_id, payload) {
        return gone;
    }
    let g = jobs::jobs();
    match job_id(payload).and_then(Value::as_str) {
        // `no_job` above has already established that the row exists and is ours.
        Some(jid) => match g.get(jid).filter(|j| &j.owner == agent_id) {
            Some(j) => json!({"jobs": [jobs::snap(jid, j, true)]}),
            None => json!({"jobs": []}),
        },
        None => json!({"jobs": g.iter()
            .filter(|(_, j)| &j.owner == agent_id)
            .map(|(id, j)| jobs::snap(id, j, false))
            .collect::<Vec<_>>()}),
    }
}

/// `clear` — forget rows of this agent.
///
/// NAMED, it drops that job whatever its status (A4.22): python's filter is
/// `k == jid if jid else v["status"] not in ("running", "stopping")`, so the status
/// label only decides which jobs a BARE `clear` sweeps. `clear` is also the one
/// caller that wants the label rather than process liveness — it is deciding what is
/// finished enough to forget.
fn clear_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let want = job_id(payload);
    let mut g = jobs::jobs();
    let doomed: Vec<String> = g
        .iter()
        .filter(|(id, j)| {
            &j.owner == agent_id
                && match want {
                    Some(w) => w.as_str() == Some(id.as_str()),
                    None => j.status != "running" && j.status != "stopping",
                }
        })
        .map(|(id, _)| id.clone())
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

fn rand_job_id() -> u32 {
    // Monotonic + pid is enough: ids only need to be unique within this process.
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    N.fetch_add(1, Ordering::Relaxed) ^ (std::process::id().wrapping_mul(2_654_435_761))
}
