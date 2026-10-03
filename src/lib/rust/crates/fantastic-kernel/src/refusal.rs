//! The ONE shape every refused call answers with.
//!
//! Port of python `kernel/_refusal.py` (canonical). Before this there were ~18
//! shapes across the two runtimes: `{"error": str}`, `{"error","reason","side",
//! "hint"}`, `{"error","created":false}`, `{"error","locked":true}`. A caller —
//! human, LLM, or another runtime — had to know which verb it had called to know
//! which shape came back, and a machine could not branch on the failure at all
//! without string-matching the prose.
//!
//! So: one object, one machine field, one human field.
//!
//! ```text
//! {"error":  "<one line a person reads>",
//!  "reason": "<code a machine branches on>",
//!  "hint":   "<what to do instead>",    // optional but strongly encouraged
//!  "see":    "<readme key>",            // optional doc pointer
//!  ...extra}                            // facts specific to this refusal
//! ```
//!
//! The machine field is `reason`, which ALREADY EXISTS — the call gate and every
//! io leg answer `reason:"unauthorized"`, and the relay bundle answers its own
//! transport codes there. Nothing is renamed. What was actually missing is that
//! most refusals carried no machine field at all: `{"error": "create_agent:
//! handler_module required"}` could only be branched on by matching prose.
//!
//! The set is CLOSED — a new code is a deliberate addition here, not an ad-hoc
//! string at a call site.
//!
//! The `hint` is not decoration. Measured 2026-08-04: a model copied the literal
//! placeholder `<id>` out of a prompt example, received `{"error": "no agent
//! '<id>'"}` thirty-four times in one run, and never once corrected. The error
//! stated a fact and withheld every fact that would have repaired it. A refusal
//! that cannot be acted on costs the same as no reply.

use fantastic_bundle::py_repr;
use serde_json::{Map, Value};

/// The call gate said no (carries `side`).
pub const UNAUTHORIZED: &str = "unauthorized";
/// No agent by that id (carries `agents`).
pub const UNKNOWN_AGENT: &str = "unknown_agent";
/// A required argument was absent (carries `arg`).
pub const MISSING_ARG: &str = "missing_arg";
/// Present but the wrong type or value (carries `arg`).
pub const BAD_ARG: &str = "bad_arg";
/// The bundle's own `validate_meta` refused.
pub const INVALID_META: &str = "invalid_meta";
/// `delete_lock` (carries `id`).
pub const LOCKED: &str = "locked";
/// Right verb, wrong agent — kernel verbs are root-only.
pub const NOT_HERE: &str = "not_here";
/// An id that is already taken (carries `id`).
pub const EXISTS: &str = "exists";
/// The agent has no such verb (carries `verb`).
pub const UNHANDLED: &str = "unhandled";
/// The named THING is absent — a path, a job, a key. NOT `unknown_agent`: the
/// agent answered, and what it was asked for is not there. Python has carried
/// this code since the set was written; the ports omitted it, so every bundle
/// that needed it (exec_bridge's `no job`, the file bundles' missing path)
/// answered a bare `{error}` and a caller branching on `reason` never matched.
pub const NOT_FOUND: &str = "not_found";

// Transport codes — a leg that could not carry the call at all. Owned by the
// bridge/relay bundles; listed here because they share the field, and a caller
// branching on `reason` must be able to see the whole vocabulary in one place.

/// The leg has no live connection.
pub const NOT_CONNECTED: &str = "not_connected";
/// The leg failed mid-carry.
pub const TRANSPORT_ERROR: &str = "transport_error";
/// The leg closed under the call.
pub const TRANSPORT_CLOSED: &str = "transport_closed";
/// The call outlived its deadline.
pub const TIMEOUT: &str = "timeout";

/// The kernel is stopping; no new roots are accepted.
pub const DRAINING: &str = "draining";
/// `watch`: the watcher has no registered inbox, so nothing would read what is
/// mirrored there. Registering a consumer is what `watch` requires, not what it
/// does.
pub const NO_CONSUMER: &str = "no_consumer";

/// The closed set. Each names a KIND of refusal, not a call site — two verbs
/// that fail for the same reason share a code, and a caller branches on the code.
pub const CODES: [&str; 16] = [
    UNAUTHORIZED,
    UNKNOWN_AGENT,
    MISSING_ARG,
    BAD_ARG,
    INVALID_META,
    LOCKED,
    NOT_HERE,
    EXISTS,
    UNHANDLED,
    NOT_FOUND,
    NOT_CONNECTED,
    TRANSPORT_ERROR,
    TRANSPORT_CLOSED,
    TIMEOUT,
    DRAINING,
    NO_CONSUMER,
];

/// How many live ids an `unknown_agent` refusal carries. Enough to name the
/// legal values, bounded so a big tree cannot turn one typo into a context dump.
pub const UNKNOWN_AGENT_IDS_MAX: usize = 40;

/// Build the one refusal shape.
///
/// `code` must be one of [`CODES`] — an unknown code is a programming error and
/// PANICS. The point of a closed set is that it cannot quietly grow at a call
/// site; in python this raises `ValueError`, and the rust twin is a debug-visible
/// panic rather than a silently-wrong wire shape.
#[must_use]
pub fn refusal(code: &str, error: impl Into<String>) -> Value {
    assert!(
        CODES.contains(&code),
        "unknown refusal code {code:?}; one of {CODES:?}"
    );
    let mut out = Map::new();
    out.insert("error".to_string(), Value::String(error.into()));
    out.insert("reason".to_string(), Value::String(code.to_string()));
    Value::Object(out)
}

/// `refusal`, plus `hint` — what to do instead. Prefer this over the bare form
/// wherever a repair exists: a refusal that cannot be acted on costs the same as
/// no reply.
#[must_use]
pub fn refusal_hint(code: &str, error: impl Into<String>, hint: impl Into<String>) -> Value {
    let mut v = refusal(code, error);
    with(&mut v, "hint", Value::String(hint.into()));
    v
}

/// Add one fact to a refusal. Facts specific to this refusal ride at the top
/// level beside `reason` — a caller reads `arg`/`side`/`id` without unwrapping.
pub fn with(target: &mut Value, key: &str, value: Value) {
    if let Some(obj) = target.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
}

/// Refuse an agent id that is not ONE SAFE PATH SEGMENT. `None` when it is fine.
///
/// An agent id is not just a name — it BECOMES A DIRECTORY. The loader builds a
/// record's on-disk home by joining the id onto the store root, so an id
/// carrying `..` or a separator walks out of the store and the write, or the
/// recursive delete, lands wherever it pointed.
///
/// Measured in python 2026-08-28: an agent the gate admitted to the root loader
/// sent `forget_record` with `id: "../../x"` and recursively deleted a directory
/// OUTSIDE the store. The call gate was not bypassed; nothing validated the id.
#[must_use]
pub fn guard_agent_id(verb: &str, agent_id: &str) -> Option<Value> {
    guard_agent_id_as(verb, "id", agent_id)
}

/// [`guard_agent_id`] for a field that is not an id but BECOMES A DIRECTORY too
/// — python `guard_agent_id(verb, value, arg=…)`: a record's `children_dir`
/// (`<agent>/<children_dir>/<child>`), which `create_agent` / `update_agent`
/// hold to the same rule. `arg` names the field in the refusal.
#[must_use]
pub fn guard_agent_id_as(verb: &str, arg: &str, agent_id: &str) -> Option<Value> {
    if agent_id.is_empty() {
        return Some(bad(verb, arg, "a non-empty string"));
    }
    if agent_id.contains('/') || agent_id.contains('\\') {
        return Some(bad(
            verb,
            arg,
            &format!(
                "a single path segment (no separators); got {}",
                py_repr(agent_id)
            ),
        ));
    }
    if agent_id == "." || agent_id == ".." || agent_id.contains("..") {
        return Some(bad(
            verb,
            arg,
            &format!(
                "a name that is not a path traversal; got {}",
                py_repr(agent_id)
            ),
        ));
    }
    if agent_id.starts_with('.') {
        return Some(bad(
            verb,
            arg,
            &format!("a name that is not hidden; got {}", py_repr(agent_id)),
        ));
    }
    if fantastic_bundle::py_isdigit(agent_id) {
        // Message ids are plain integers; in prompt text an all-digit agent id
        // would read as one. Names are words.
        return Some(bad(
            verb,
            arg,
            &format!(
                "a name, not a number (message ids are ints); got {}",
                py_repr(agent_id)
            ),
        ));
    }
    if agent_id.contains('|') {
        // `|` qualifies a FOREIGN agent (`<guid>|<agent>`): a local id never
        // carries one, so a gate can tell local from foreign by the id alone.
        return Some(bad(
            verb,
            arg,
            &format!(
                "a local name without `|` (it marks a foreign `guid|agent`); got {}",
                py_repr(agent_id)
            ),
        ));
    }
    if agent_id == RESERVED_ID {
        // `kernel` names the operator: sends to it reach the root, and a memory
        // store serves the operator under it. An agent holding that id would BE
        // the operator to every store (measured in python 2026-09-28).
        return Some(bad(
            verb,
            arg,
            &format!("a name other than the reserved {}", py_repr(RESERVED_ID)),
        ));
    }
    None
}

/// The one reserved agent id — the operator's name (send target, store identity).
pub const RESERVED_ID: &str = "kernel";

/// `<verb>: <arg> required` — the commonest refusal, spelled once.
#[must_use]
pub fn missing(verb: &str, arg: &str) -> Value {
    let mut v = refusal(MISSING_ARG, format!("{verb}: {arg} required"));
    with(&mut v, "arg", Value::String(arg.to_string()));
    v
}

/// `missing` WITH a recipe. Use this rather than `refusal_hint(MISSING_ARG, …)`:
/// the hand-rolled form drops the machine-readable `arg`, which is the whole
/// point of the code. Measured by the shared C21 corpus 2026-08-27 —
/// `create_agent: parent_id required` carried a hint and no `arg`, so python's
/// reply and ours disagreed on a field a caller branches on.
///
/// KEY ORDER matches python's `refusal(code, error, hint=hint, arg=arg)`:
/// `error, reason, hint, arg` — `hint` is a named parameter of `refusal()`
/// itself and is inserted before `**extra` (which is where `arg` rides), so
/// hint precedes arg. `missing()` alone has no hint and stays `error, reason,
/// arg`.
#[must_use]
pub fn missing_hint(verb: &str, arg: &str, hint: impl Into<String>) -> Value {
    let mut v = refusal(MISSING_ARG, format!("{verb}: {arg} required"));
    with(&mut v, "hint", Value::String(hint.into()));
    with(&mut v, "arg", Value::String(arg.to_string()));
    v
}

/// Present but unusable — say what was WANTED, not just that it was wrong.
#[must_use]
pub fn bad(verb: &str, arg: &str, want: &str) -> Value {
    let mut v = refusal(BAD_ARG, format!("{verb}: {arg} must be {want}"));
    with(&mut v, "arg", Value::String(arg.to_string()));
    v
}

/// Right verb, wrong agent. Kernel verbs act on the TREE, so the agent named is
/// the OPERAND, not the actor — the refusal says where to send it instead.
///
/// PYTHON'S PROSE, verbatim, plus the machine field. The two runtimes said the
/// same thing in different words on the same `not_here` code, and the ports
/// dropped `verb` entirely — so a caller could branch on the code but not learn
/// WHICH verb it had misaddressed without parsing the sentence.
#[must_use]
pub fn not_here(verb: &str, id: &str) -> Value {
    let mut v = refusal_hint(
        NOT_HERE,
        format!("{verb} is a kernel verb — {id} does not answer it"),
        "address the kernel root ('kernel'); name the agent you mean in \
         `parent_id` / `id`",
    );
    with(&mut v, "verb", Value::String(verb.to_string()));
    v
}

/// The reply for an id that names no agent — carrying the LIVE id index.
///
/// Reading the flat agents index costs nothing (it is already maintained, so
/// this is a map walk, not a reflect), and a bare `no agent 'x'` teaches the
/// caller nothing it can act on. Same principle the ingress rules already
/// follow: a denial names the legal values.
///
/// This is NOT a repair and NOT a fallback — the call still fails, exactly once,
/// by the one path. It just says what the legal ids are on the way out.
#[must_use]
pub fn unknown_agent(agent_id: &str, mut live_ids: Vec<String>) -> Value {
    live_ids.sort();
    let count = live_ids.len();
    live_ids.truncate(UNKNOWN_AGENT_IDS_MAX);
    let mut v = refusal(UNKNOWN_AGENT, format!("no agent {}", py_repr(agent_id)));
    with(&mut v, "agent_count", Value::from(count));
    with(
        &mut v,
        "agents",
        Value::Array(live_ids.into_iter().map(Value::String).collect()),
    );
    // An id wrapped in angle brackets is structurally a SLOT, not a name — no id
    // this kernel mints can contain them (`{bundle}_{hex6}` or a bundle name), so
    // this costs no guessing.
    if agent_id.starts_with('<') && agent_id.ends_with('>') {
        with(
            &mut v,
            "hint",
            Value::String(format!(
                "{} is a PLACEHOLDER from an example, not an id — \
                 substitute one of `agents`.",
                py_repr(agent_id)
            )),
        );
    }
    v
}

#[cfg(test)]
#[path = "refusal/tests.rs"]
mod tests;
