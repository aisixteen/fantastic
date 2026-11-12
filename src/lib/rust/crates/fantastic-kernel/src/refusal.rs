//! The ONE shape every refused call answers with.
//!
//! Port of python `kernel/_refusal.py` (canonical). Without one shape a caller —
//! human, LLM, or another runtime — has to know which verb it called to know
//! which shape comes back, and a machine cannot branch on the failure at all
//! without string-matching the prose.
//!
//! So: one object, one machine field, one human field.
//!
//! ```text
//! {"error":  "<one line a person reads>",
//!  "reason": "<code a machine branches on>",
//!  "hint":   "<what to do instead>",    // optional but strongly encouraged
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
//! The `hint` is not decoration: a model that copies a literal placeholder like
//! `<id>` out of a prompt example and receives a bare `{"error": "no agent
//! '<id>'"}` repeats it without ever correcting — the error states a fact and
//! withholds every fact that would repair it. A refusal
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

// Bundle codes — the generic kinds bundles refuse with. Listed here because the
// set is closed and a caller branches on `reason` across every bundle.

/// A spawn or quota past a configured limit (carries `scope`, `role`, `running`, `max`).
pub const LIMIT: &str = "limit";
/// The caller does not hold what it tried to grant or fork.
pub const NOT_ALLOWED: &str = "not_allowed";
/// A grant could not be written; the whole operation is undone.
pub const GRANT_FAILED: &str = "grant_failed";
/// A card names a tier the registry does not have (carries `tier`).
pub const UNKNOWN_TIER: &str = "unknown_tier";
/// A tier is present but unusable.
pub const BAD_TIER: &str = "bad_tier";
/// The kernel has no root.
pub const NO_ROOT: &str = "no_root";
/// The caller was not spawned by this registry.
pub const NOT_SPAWNED: &str = "not_spawned";
/// The bundle's record lacks the config that turns it on.
pub const DISABLED: &str = "disabled";
/// The backing store refused or failed the call (carries `store_error`, `store_reason`).
pub const STORE_REFUSED: &str = "store_refused";
/// The task already has a holder.
pub const ALREADY_HELD: &str = "already_held";
/// The task has no holder.
pub const NOT_HELD: &str = "not_held";
/// The named agent is not the caller or one of its descendants.
pub const NOT_IN_SUBTREE: &str = "not_in_subtree";

/// The closed set. Each names a KIND of refusal, not a call site — two verbs
/// that fail for the same reason share a code, and a caller branches on the code.
pub const CODES: [&str; 28] = [
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
    LIMIT,
    NOT_ALLOWED,
    GRANT_FAILED,
    UNKNOWN_TIER,
    BAD_TIER,
    NO_ROOT,
    NOT_SPAWNED,
    DISABLED,
    STORE_REFUSED,
    ALREADY_HELD,
    NOT_HELD,
    NOT_IN_SUBTREE,
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

/// The error text of a reply, or `None` when the reply carries no `error`.
/// A string `error` is returned as is (any other JSON shape as its JSON text);
/// a string `message` is appended as `: {message}`.
#[must_use]
pub fn error_text(reply: &Value) -> Option<String> {
    let e = reply
        .get("error")
        .filter(|e| !e.is_null() && e.as_str() != Some(""))?;
    let mut s = e
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| e.to_string());
    if let Some(m) = reply.get("message").and_then(Value::as_str) {
        s = format!("{s}: {m}");
    }
    Some(s)
}

/// Does the reply carry an `error` (present, not null, not empty text)?
#[must_use]
pub fn is_refused(reply: &Value) -> bool {
    error_text(reply).is_some()
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

/// The one reserved agent id — the operator's name (send target, store identity).
pub const RESERVED_ID: &str = "kernel";

/// `<verb>: <arg> required` — the commonest refusal, spelled once.
#[must_use]
pub fn missing(verb: &str, arg: &str) -> Value {
    let mut v = refusal(MISSING_ARG, format!("{verb}: {arg} required"));
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
