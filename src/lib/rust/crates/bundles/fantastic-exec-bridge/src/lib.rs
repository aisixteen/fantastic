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
//! `[4-byte BE length | JSON]` — and nothing else. A program that wants a
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
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
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
/// than by discipline: `[4-byte BE length | JSON body]`, with `FANTASTIC_CTRL_FD` /
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
/// else via its text form.
fn value_to_pattern(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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
        if let Ok(re) = regex::Regex::new(&format!(r"\A(?s:{pat})\z")) {
            if re.is_match(line) {
                return None;
            }
        }
    }
    Some(format!(
        "exec_bridge: command not allowed: {line:?} — this leg permits: {pats:?}"
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
        if let Some(spec) = meta.get("ingress_rule") {
            let resolved = fantastic_io_bridge::authorizer::ingress::resolve(Some(spec));
            let known = matches!(
                spec.as_str(),
                Some("allow_all_ingress") | Some("deny_ingress") | Some("password_ingress")
            ) || resolved.is_ok() && spec.is_object();
            if !known {
                return Some(format!(
                    "ingress_rule={spec} is not a rule this bundle implements — \
                     legal values: allow_all_ingress, deny_ingress, password_ingress"
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
                    "allowed must be a non-empty list of regexes, e.g. allowed=['ls( .*)?']"
                        .to_string(),
                );
            }
            for pat in &pats {
                if let Err(e) = regex::Regex::new(pat) {
                    return Some(format!("allowed contains a broken regex {pat:?}: {e}"));
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
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        // AUTH GATE — the process edge, gated like any io_bridge leg (SEALED by
        // default). `reflect` is UNGATED: discovery must work on a sealed bridge.
        // Mail (the unknown-verb branch below) is deliberately not gated, matching
        // py: the substrate's fanout already delivered the payload; this only ACKs.
        let owned = matches!(
            verb,
            "start" | "boot" | "shutdown" | "status" | "clear" | "interrupt" | "stop"
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
            "shutdown" => Value::Null,
            "status" => status_reply(agent_id, payload),
            "clear" => clear_reply(agent_id, payload),
            "interrupt" => interrupt_reply(agent_id, payload),
            "stop" => stop_reply(agent_id, payload),
            // ADDRESSABLE INBOX. Any verb this bundle does not own is mail for the
            // running program — the substrate's fanout has ALREADY delivered it to the
            // job's `_xbself_` proxy, so this only ACKs. Re-emitting would double it.
            other => {
                if jobs::has_running_job(agent_id, kernel) {
                    json!({"queued": true})
                } else {
                    json!({"error": format!(
                        "exec_bridge: unknown type {other:?} (no running job to receive it)"
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
        token: meta.get("auth_token").and_then(Value::as_str),
    };
    if let fantastic_io_bridge::authorizer::Decision::Deny(reason) =
        fantastic_io_bridge::gate_inbound(&rule, &action)
    {
        return Some(json!({
            "error": reason,
            "reason": "unauthorized",
            "hint": "the process edge is sealed; open it: update_agent <id> ingress_rule=allow_all_ingress",
        }));
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

/// Read a string field, treating empty strings as absent. Callers
/// (`update_agent id=py python=""`) pass `""` to unset — without this
/// filter the resolver picks up the empty path and spawn fails with
/// ENOENT. Discovered by the resolution selftests (Tests 4/5/6/8).
fn meta_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn payload_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

// ── verb implementations ────────────────────────────────────────────

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let cwd_val = meta_str(&meta, "cwd")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "<process default>".to_string());
    // Liveness comes off the JOB TABLE — one registry, no second truth to drift.
    let running = jobs::jobs()
        .values()
        .filter(|j| &j.owner == agent_id && j.exit_code.is_none())
        .count();
    let ingress = fantastic_io_bridge::authorizer::rule_name(
        fantastic_io_bridge::authorizer::manifold_spec(
            meta.get("ingress_rule"),
            meta.get("auth"),
            "_ingress",
        )
        .as_ref(),
        "deny_ingress",
    );
    json!({
        "id": agent_id.as_str(),
        // Py-canonical sentence — the ALLOWLIST is advertised so an authoring
        // model can see what it may run without guessing.
        "sentence": "Process edge of the io family — a general-purpose COMMAND runner, sealed by default, gated by the `allowed` regexes. Spawns commands in the background; a child may speak the kernel over a private control fd.",
        "cwd": cwd_val,
        // THE GATE, advertised: a caller must see what it may run.
        "allowed": allowed_patterns(&meta),
        "running": running,
        "verbs": {
            "reflect": "Identity + running job count + auth posture. No args.",
            "start": "args: command:str (req), args:list[str]? (argv, default []), env:dict? (extra env vars), cwd:str?. The FULL line (command + args) must fullmatch one of the record's `allowed` regexes or the start is refused naming the patterns. Spawns in the BACKGROUND; returns {job_id, status:'running'} at once and streams progress/job_done.",
            "status": "args: job_id:str?. One job (with full output) or all of this agent's jobs.",
            "clear": "args: job_id:str?. Drop finished rows of this agent.",
            "interrupt": "args: job_id:str?. SIGINT in-flight subprocesses. Returns {interrupted:int}.",
            "stop": "args: job_id:str?. SIGKILL in-flight subprocesses. Returns {killed:int}.",
            "boot": "No-op. Nothing auto-restarts: a command is started by a caller, not the record.",
        },
        "emits": {
            "job_started": "{type:'job_started', job_id}",
            "progress": "{type:'progress', job_id, n, line, stream} — one per output line",
            "job_done": "{type:'job_done', job_id, exit_code, ok, stdout, stderr}",
        },
        "wire": "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD, a BIDIRECTIONAL socket (one end of a socketpair): frame is [4-byte BE length | JSON], both directions. To CALL another agent: write {op:'send', rid:<your id>, target, payload} and the reply COMES BACK on the SAME fd as {op:'reply', rid:<same id>, data} — the fd is not one-way. It is not inline, though: read frames in a loop and skip any whose rid isn't yours (event/inbox frames can arrive first) until you see your reply. ops: send/emit/reflect/watch/on_message. A command that never reads the fd runs exactly like any OS process.",
        "inbox": "Any verb NOT listed in `verbs` is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is an error — a script that only defines something exits before its first turn arrives.",
        "tasks": "Every running job holds a row in the kernel TASK TABLE (`task_id` in each `status` snapshot; listed by the universal `ps`). `cancel task_id=<id>` kills it — the OS subprocess is SIGKILLed by this bundle's `on_cancel` hook before the kernel aborts the task, so nothing is orphaned.",
        "authoring": "A general-purpose command runner: `start {command, args}`, gated by the `allowed` regexes on the full command line.",
        "ingress_rule": ingress,
        "sealed": ingress != "allow_all_ingress",
    })
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
    let want = payload.get("job_id").and_then(Value::as_str);
    let mut g = jobs::jobs();
    let mut n = 0u64;
    for (id, j) in g.iter_mut() {
        if &j.owner != agent_id || want.is_some_and(|w| w != id.as_str()) {
            continue;
        }
        if j.exit_code.is_some() {
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

fn interrupt_reply(agent_id: &AgentId, payload: &Value) -> Value {
    json!({"interrupted": signal_jobs(agent_id, payload, nix::sys::signal::Signal::SIGINT)})
}

fn stop_reply(agent_id: &AgentId, payload: &Value) -> Value {
    json!({"killed": signal_jobs(agent_id, payload, nix::sys::signal::Signal::SIGKILL)})
}

/// `start` — spawn the COMMAND in the BACKGROUND and return at once.
///
/// args: `command:str` (req), `args:list[str]?` (argv, default `[]`), `env:dict?`
/// (extra env vars), `cwd:str?`. The FULL line (command + args, shell-joined) must
/// fullmatch one of the record's `allowed` regexes or the start is refused naming
/// the patterns. A `code` field is refused explicitly — this runs COMMANDS, not code.
async fn start_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    // `code` is not a field — run a command instead (py tools.py `_start`).
    if payload.get("code").is_some_and(|v| !v.is_null()) {
        return json!({"error": "exec_bridge: `code` is not a field — run a command instead"});
    }
    let command = match payload.get("command").and_then(Value::as_str) {
        Some(c) if !c.trim().is_empty() => c.to_string(),
        _ => return json!({"error": "exec_bridge: command (str) required"}),
    };
    // `args` — a list of scalars (str/int/float), each stringified into argv.
    let raw_args = payload.get("args").cloned().unwrap_or_else(|| json!([]));
    let args: Vec<String> = match &raw_args {
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                match v {
                    Value::String(s) => out.push(s.clone()),
                    Value::Number(n) => out.push(n.to_string()),
                    _ => {
                        return json!({"error":
                            "exec_bridge: start expects args to be a list of strings"})
                    }
                }
            }
            out
        }
        _ => return json!({"error": "exec_bridge: start expects args to be a list of strings"}),
    };
    // The FULL argv, shell-joined, matched against the allowlist BEFORE spawning.
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(command.clone());
    argv.extend(args.iter().cloned());
    let line = shlex_join(&argv);
    if let Some(why) = command_permitted(&meta, &line) {
        return json!({ "error": why });
    }
    // The per-call `env` block — a dict of str -> scalar.
    let env_extra: Map<String, Value> = match payload.get("env") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => {
            for v in m.values() {
                if !matches!(v, Value::String(_) | Value::Number(_)) {
                    return json!({"error":
                        "exec_bridge: start expects env to be a dict of str -> str"});
                }
            }
            m.clone()
        }
        _ => return json!({"error": "exec_bridge: start expects env to be a dict of str -> str"}),
    };
    let cwd = payload_str(payload, "cwd").or_else(|| meta_str(&meta, "cwd"));

    let spawned = match crate::spawn::spawn_job(&argv, agent_id.as_str(), &env_extra, cwd) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("exec_bridge: spawn failed: {e}")}),
    };
    let crate::spawn::Spawned { child, ctrl } = spawned;
    let Some(pid) = child.id() else {
        return json!({"error": "exec_bridge: child exited before it could be tracked"});
    };
    let job_id = format!("{:08x}", rand_job_id());

    let (rd, wr) = ctrl.into_split();
    let ctrl_task = tokio::spawn(crate::relay::serve(
        rd,
        wr,
        agent_id.clone(),
        job_id.clone(),
        Arc::clone(kernel),
    ));

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
            // WHAT ran — so a caller can re-issue this job verbatim.
            command,
            args,
            cwd: cwd.map(str::to_string),
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
    let g = jobs::jobs();
    match payload.get("job_id").and_then(Value::as_str) {
        Some(jid) => match g.get(jid).filter(|j| &j.owner == agent_id) {
            Some(j) => json!({"jobs": [jobs::snap(jid, j, true)]}),
            None => json!({"error": format!("exec_bridge: no job {jid:?}")}),
        },
        None => json!({"jobs": g.iter()
            .filter(|(_, j)| &j.owner == agent_id)
            .map(|(id, j)| jobs::snap(id, j, false))
            .collect::<Vec<_>>()}),
    }
}

/// `clear` — drop finished rows of this agent. Running ones stay.
fn clear_reply(agent_id: &AgentId, payload: &Value) -> Value {
    let want = payload.get("job_id").and_then(Value::as_str);
    let mut g = jobs::jobs();
    let doomed: Vec<String> = g
        .iter()
        .filter(|(id, j)| {
            &j.owner == agent_id
                && want.is_none_or(|w| w == id.as_str())
                && j.status != "running"
                && j.status != "stopping"
        })
        .map(|(id, _)| id.clone())
        .collect();
    let n = doomed.len();
    for id in doomed {
        g.remove(&id);
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
