//! Process edge of the io family — rust mirror of the canonical python
//! `io/exec_bridge` (which is canonical; on disagreement this crate is wrong).
//!
//! Crossing kernel↔subprocess is gated like any io_bridge leg: **SEALED BY
//! DEFAULT** (absent `ingress_rule` ⇒ every verb except `reflect` denies
//! `{reason:"unauthorized"}`), and bound by the **interpreter ALLOWLIST**:
//! `interpreters` on the record is the only set that can be spawned. No per-call
//! override, no default, no PATH fallback — an unlisted `interpreter` is refused
//! at WRITE time (`validate_meta`) and again at `start`, naming the legal set.
//!
//! The bridge publishes a WIRE — the inherited `FANTASTIC_CTRL_FD`, framed
//! `[4-byte BE length | JSON]` — and nothing else. `shim:'python'` prepends the
//! wire client ([`CONNECTOR_SRC`]); `shim` stays OPTIONAL because the wire is
//! the contract, so adding a language is adding a blob to [`SHIMS`], not a code
//! path. `start` spawns in the BACKGROUND and returns `{job_id,
//! status:"running"}` at once, streaming `progress` / `job_done`; `boot`
//! re-runs the persisted `code` after a reboot and PROPAGATES its error instead
//! of booting silently broken. Any verb this bundle does not own is MAIL for
//! the running program (`{queued:true}`); jobs are kernel TASKS (`ps`/`cancel`,
//! `on_cancel` SIGKILLs the real process).
//!
//! **NOT a sandbox.** An allowlisted interpreter is a NAMED interpreter, with
//! all of that interpreter's reach — a CPython on the list still has `open()`,
//! `socket`, `subprocess`. The allowlist buys exactly one property: the
//! OPERATOR chose which runtime and which environment a program runs in, and
//! the program cannot choose a different one. Anything stronger needs an OS
//! mechanism (`sandbox-exec`, a separate uid, a container) and is DEFERRED.
//!
//! ## Verbs
//!
//! | verb       | args                                                            | reply                                          |
//! |------------|-----------------------------------------------------------------|------------------------------------------------|
//! | `reflect`  | _none_ (UNGATED — discovery works on a sealed leg)              | identity + allowlist + shims + posture         |
//! | `start`    | `code:str?` (else the record's), `args:any?`, `cwd?`, `interpreter:str?` (one of the allowlist), `shim:str?` | `{job_id, status:"running"}` at once |
//! | `status`   | `job_id:str?`                                                   | one job with its output, or all of this agent's |
//! | `clear`    | `job_id:str?`                                                   | drops finished rows of this agent              |
//! | `interrupt`| `job_id:str?`                                                   | `{interrupted: int}` (SIGINT to in-flight)     |
//! | `stop`     | `job_id:str?`                                                   | `{killed: int}` (SIGKILL to in-flight)         |
//! | `boot`     | _none_                                                          | re-starts the persisted `code`; ERRORS propagate |
//!
//! Anything else is MAIL for the running program: `{queued:true}` when a job is
//! live, a loud error naming the verb when none is.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// The child-side connector, VENDORED VERBATIM from python's
/// `exec_bridge/_connector.py`.
///
/// It is pure-stdlib Python (`os, json, socket, struct, threading, queue, sys`) with
/// zero dependency on the python kernel. Each runtime's exec bridge prepends this
/// identical text (`shim:'python'`) and implements only the PARENT half of the control
/// socket. That makes the wire identical by construction rather than by discipline:
/// `[4-byte BE length | JSON body]`, with `FANTASTIC_CTRL_FD` / `FANTASTIC_SELF_ID` /
/// `FANTASTIC_ARGS` in the child env.
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

/// name → source prepended to the program. One entry per LANGUAGE, not per
/// interpreter: every CPython on an allowlist shares this client. Adding a
/// language is adding a blob here, not a code path.
pub const SHIMS: &[(&str, &str)] = &[("python", CONNECTOR_SRC)];

/// The shim names this bridge can serve — what `reflect` advertises.
pub fn shim_names() -> Vec<&'static str> {
    SHIMS.iter().map(|(n, _)| *n).collect()
}

/// `program`, with the named shim prepended. `None` ⇒ untouched (raw wire).
/// An unknown name is a REFUSAL naming the legal set, not a silent pass-through:
/// a program that asked for `kernel` and did not get it fails later, somewhere
/// else, with a `NameError` that names nothing useful.
pub fn apply_shim(shim: Option<&str>, program: &str) -> Result<String, String> {
    match shim {
        None | Some("") => Ok(program.to_string()),
        Some(name) => match SHIMS.iter().find(|(n, _)| *n == name) {
            Some((_, blob)) => Ok(format!("{blob}\n{program}")),
            None => Err(format!(
                "exec_bridge: shim {name:?} is unknown — this bridge serves {:?}",
                shim_names()
            )),
        },
    }
}

// ── the interpreter ALLOWLIST (py `proc.allowed` / `proc.resolve`) ──

/// This leg's allowlisted interpreters, `~` expanded. Empty ⇒ the leg spawns
/// nothing (a legal state: wired but given nothing to run, refusing loudly).
pub fn allowed(meta: &Map<String, Value>) -> Vec<String> {
    let raw = match meta.get("interpreters") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => vec![],
    };
    raw.iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| expanduser(s).to_string_lossy().to_string())
        .collect()
}

/// The interpreter to spawn, or refuse NAMING the legal set — the mechanism
/// measured (2026-08-06) to let a small model recover: a refusal that does not
/// say what WOULD work only teaches that something is wrong. Requesting nothing
/// is legal when the leg lists exactly one interpreter; with several, silence
/// is ambiguous and refused rather than guessed.
pub fn resolve_interpreter(
    meta: &Map<String, Value>,
    requested: Option<&str>,
) -> Result<String, String> {
    let ok = allowed(meta);
    if ok.is_empty() {
        return Err(
            "no interpreters are allowlisted on this leg — an operator must set \
             `interpreters=[<abs path to an interpreter>]` before a program can run"
                .to_string(),
        );
    }
    match requested.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            if ok.len() == 1 {
                Ok(ok[0].clone())
            } else {
                Err(format!(
                    "this leg allows {} interpreters, so name one: interpreter=<one of {ok:?}>",
                    ok.len()
                ))
            }
        }
        Some(req) => {
            let want = expanduser(req).to_string_lossy().to_string();
            if ok.contains(&want) {
                Ok(want)
            } else {
                Err(format!("interpreter {req:?} is not allowlisted — {ok:?}"))
            }
        }
    }
}

/// The full argv: `argv_template` on the record with `<interp>`/`<program>`
/// substituted. A template rather than hardcoded flags because `-u -c` is
/// CPython-specific (node wants `-e`); the bridge does not know which language
/// it is spawning and must not pretend to. Default is the CPython shape — the
/// only family this bundle ships a shim for.
pub fn build_argv(meta: &Map<String, Value>, interp: &str, program: &str) -> Vec<String> {
    let default = vec![
        json!("<interp>"),
        json!("-u"),
        json!("-c"),
        json!("<program>"),
    ];
    let tmpl: Vec<Value> = match meta.get("argv_template") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => vec![json!(s)],
        _ => default,
    };
    tmpl.iter()
        .map(|a| match a.as_str() {
            Some("<interp>") => interp.to_string(),
            Some("<program>") => program.to_string(),
            Some(other) => other.to_string(),
            None => a.to_string(),
        })
        .collect()
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
    /// (py-canonical `validate_meta`). Validates the MERGED state — `record`
    /// (empty at create) with `meta` applied — because a cross-field rule
    /// (`interpreter` against `interpreters`) is unenforceable from the patch
    /// alone.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        record: &Map<String, Value>,
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
        if let Some(shim) = meta.get("shim").and_then(Value::as_str) {
            if !shim.is_empty() && !SHIMS.iter().any(|(n, _)| *n == shim) {
                return Some(format!(
                    "shim={shim:?} is unknown — this bridge serves {:?}",
                    shim_names()
                ));
            }
        }
        if let Some(raw) = meta.get("interpreters") {
            let ok = matches!(raw, Value::String(s) if !s.trim().is_empty())
                || matches!(raw, Value::Array(a) if a
                    .iter()
                    .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())));
            if !ok {
                return Some(
                    "interpreters must be a list of interpreter paths, e.g. \
                     interpreters=['~/.venvs/x/bin/python']"
                        .to_string(),
                );
            }
        }
        // `interpreter` against the allowlist the record WILL have.
        let mut merged = record.clone();
        for (k, v) in meta {
            merged.insert(k.clone(), v.clone());
        }
        if let Some(req) = merged.get("interpreter").and_then(Value::as_str) {
            if !req.is_empty() {
                if let Err(e) = resolve_interpreter(&merged, Some(req)) {
                    return Some(e);
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

/// Expand a leading `~` to `$HOME`. Pure on platforms without HOME.
fn expanduser(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~") {
        if let Ok(home) = std::env::var("HOME") {
            let trimmed = rest.strip_prefix('/').unwrap_or(rest);
            return PathBuf::from(home).join(trimmed);
        }
    }
    PathBuf::from(s)
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
        // model never has to GUESS an interpreter path (a guess has no legal
        // answer, same defect class as an unnamed executor module).
        "sentence": "Process edge of the io family — sealed by default, allowlisted interpreters only. Spawns a program in the background; it gets a kernel wire over a private control fd.",
        "cwd": cwd_val,
        "interpreters": allowed(&meta),
        "shims": shim_names(),
        "running": running,
        "verbs": {
            "reflect": "Identity + allowlist + shims + running job count + auth posture. No args.",
            "start": "args: code:str? (falls back to the record's `code`), args:any? (THIS CALL's arguments — reaches the program as `kernel.args`), cwd:str?, interpreter:str? (one of the allowlist; optional when it lists exactly one), shim:str? (one of `shims`; absent ⇒ the program speaks the raw wire). Spawns in the BACKGROUND; returns {job_id, status:'running'} at once and streams progress/job_done.",
            "status": "args: job_id:str?. One job (with full output) or all of this agent's jobs.",
            "clear": "args: job_id:str?. Drop finished rows of this agent.",
            "interrupt": "args: job_id:str?. SIGINT in-flight subprocesses. Returns {interrupted:int}.",
            "stop": "args: job_id:str?. SIGKILL in-flight subprocesses. Returns {killed:int}.",
            "boot": "No args. Re-runs the record's persisted `code` (unless a job is live). Its ERROR propagates — a record naming a non-allowlisted interpreter must not boot silently broken.",
        },
        "emits": {
            "job_started": "{type:'job_started', job_id}",
            "progress": "{type:'progress', job_id, n, line, stream} — one per output line",
            "job_done": "{type:'job_done', job_id, exit_code, ok, stdout, stderr}",
        },
        "wire": "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD: [4-byte BE length | JSON]. Name `shim:'<one of shims>'` on `start` and a client is prepended for you (python: a `kernel` global with send/emit/reflect/watch/on_message + self_id + args). Name none and speak it yourself — reflect readme=true for the frame vocabulary.",
        "inbox": "Any verb NOT listed in `verbs` is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is an error — a script that only defines something exits before its first turn arrives.",
        "tasks": "Every running job holds a row in the kernel TASK TABLE (`task_id` in each `status` snapshot; listed by the universal `ps`). `cancel task_id=<id>` kills it — the OS subprocess is SIGKILLed by this bundle's `on_cancel` hook before the kernel aborts the task, so nothing is orphaned.",
        "authoring": "This agent's persisted `code` is a durable tool and a SCRIPT: it runs TOP TO BOTTOM, once per `start` and again on `boot`. A bare `def` only defines and exits — put the work at module level, and keep the program running if it must receive later sends. create_agent with `code` + an allowlisted `interpreter`; `delete_agent` removes it. A running program can itself create_agent more agents.",
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

/// `start` — spawn the program in the BACKGROUND and return at once.
///
/// The `code` is persisted onto the record, so `boot` revives it after a reboot; the
/// LOADER lays that down as `main.py` beside `agent.json`.
async fn start_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let code = payload_str(payload, "code")
        .or_else(|| meta_str(&meta, "code"))
        .unwrap_or_default();
    if code.is_empty() {
        return json!({"error":
            "exec_bridge: code (str) required (pass it or set it on the record)"});
    }
    // PER-CALL ARGUMENTS, explicit field — implicit capture of unknown payload keys
    // would silently swallow a typo'd `cwd` and hand it over as an argument.
    let args_json = payload
        .get("args")
        .map(ToString::to_string)
        .unwrap_or_else(|| "{}".to_string());
    if args_json.len() > MAX_ARGS_BYTES {
        return json!({"error": format!(
            "exec_bridge: args too large ({} bytes > {MAX_ARGS_BYTES}) — they ride the \
             process env, which caps near 1 MB for the whole block. Put bulk data in an \
             agent and pass its id.", args_json.len())});
    }
    // THE ALLOWLIST — refuse before spawning, naming the legal set.
    let requested = payload_str(payload, "interpreter").or_else(|| meta_str(&meta, "interpreter"));
    let interp = match resolve_interpreter(&meta, requested) {
        Ok(p) => p,
        Err(e) => return json!({ "error": format!("exec_bridge: {e}") }),
    };
    // The OPTIONAL shim — unknown names refuse, absent means the raw wire.
    let shim = payload_str(payload, "shim").or_else(|| meta_str(&meta, "shim"));
    let program = match apply_shim(shim, code) {
        Ok(p) => p,
        Err(e) => return json!({ "error": e }),
    };
    let cwd = payload_str(payload, "cwd").or_else(|| meta_str(&meta, "cwd"));

    let argv = build_argv(&meta, &interp, &program);
    let spawned = match crate::spawn::spawn_job(&argv, agent_id.as_str(), &args_json, cwd) {
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
            status: "running".to_string(),
            started_at: now_secs(),
            exit_code: None,
            lines: 0,
            last_line: String::new(),
            stdout: String::new(),
            stderr: String::new(),
        },
    );
    // Durable authored agent: the record carries the program.
    if let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        agent.update_meta(serde_json::Map::from_iter([(
            "code".to_string(),
            Value::String(code.to_string()),
        )]));
        let _ = fantastic_kernel::persistence::persist(kernel, &agent).await;
    }
    kernel
        .emit(agent_id, json!({"type": "job_started", "job_id": job_id}))
        .await;
    json!({"job_id": job_id, "status": "running"})
}

/// `boot` — idempotent rehydration. A record carrying `code` becomes a living program
/// again, unless one is already running. Its ERROR PROPAGATES (py-canonical):
/// discarding it made a record naming a non-allowlisted interpreter boot, fail to
/// spawn, and still look created — indistinguishable from a program that ran and
/// did nothing.
async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    if let Some(code) = meta_str(&meta, "code") {
        if !code.is_empty() && !jobs::has_running_job(agent_id, kernel) {
            // A rehydrate has no caller, so no arguments.
            let reply = start_reply(agent_id, &json!({"code": code}), kernel).await;
            if reply.get("error").is_some() {
                return reply;
            }
        }
    }
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

/// Per-call `args` ride the process env, which caps near 1 MB for the whole block —
/// past it `execve` fails with a cryptic E2BIG. Refuse early and say why.
const MAX_ARGS_BYTES: usize = 256 * 1024;
