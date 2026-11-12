//! MAIL — what the program a job runs declared, and the verdict on each piece.
//!
//! The running program declares verbs of its own (`declare_verbs`, sent over its
//! control fd to its own agent). The agent's entry then checks a call of one of them
//! by the schema the program declared — the same engine as every other verb — and
//! the relay's inbox drain delivers to the program only mail the same entry accepts.
//! There is no other mail: a verb nobody declared is refused by name with the list the
//! agent answers.

use std::sync::LazyLock;

use fantastic_kernel::verbs::DynCall;
use fantastic_kernel::{AgentId, DynRegistry, DynVerb, Parse};
use serde_json::{json, Value};

use crate::verbs;

/// The declared verbs of every agent's running program: one list per agent, replaced
/// by each declaration and dropped when the program ends.
static REGISTRY: LazyLock<DynRegistry> = LazyLock::new(DynRegistry::new);

/// The verbs the agent already answers: a program cannot redeclare one.
const RESERVED: [&[fantastic_kernel::Verb]; 2] =
    [verbs::VERBS, fantastic_kernel::declared::common::VERBS];

/// The verbs `owner`'s program declared (empty before it declared any).
pub(crate) fn declared(owner: &AgentId) -> Vec<DynVerb> {
    REGISTRY.of(owner.as_str())
}

/// The program's end: its verbs go with it.
pub(crate) fn forget(owner: &AgentId) {
    REGISTRY.forget(owner.as_str());
}

/// `declare_verbs`: replace the program's declared verbs. Only a running program
/// declares; a verb the agent already answers cannot be redeclared.
pub(crate) fn declare_reply(owner: &AgentId, a: &verbs::DeclareVerbs) -> Value {
    if !crate::jobs::has_running_job(owner) {
        return fantastic_kernel::refusal::refusal(
            fantastic_kernel::refusal::NOT_FOUND,
            "exec_bridge: declare_verbs needs a running job — the verbs are its program's",
        );
    }
    let list = serde_json::to_value(&a.verbs).unwrap_or_default();
    match REGISTRY.declare(owner.as_str(), &list, &RESERVED) {
        Ok(()) => json!({"declared": a.verbs.iter().map(|v| v.name.as_str()).collect::<Vec<_>>()}),
        Err(refused) => refused,
    }
}

/// The mail as the program receives it and the ledger of refusals keys it: the verb
/// and its checked arguments.
pub(crate) fn payload_of(c: &DynCall) -> Value {
    let mut out = match &c.args {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    out.insert("type".to_string(), json!(c.name));
    Value::Object(out)
}

/// What the entry makes of a queued payload for `owner`'s agent: the mail it is — a
/// call of a verb the program declared, checked — or `None` (a verb the agent
/// answers itself, telemetry, a call the entry refuses).
pub(crate) fn of(owner: &AgentId, payload: &Value) -> Option<DynCall> {
    match <verbs::Call as Parse>::parse(owner.as_str(), payload, &declared(owner)) {
        Ok(verbs::Call::Dynamic(c)) => Some(c),
        _ => None,
    }
}

/// A test's stand-in for the program: declare mail verbs of `owner` (each takes any
/// arguments an object can carry).
#[cfg(test)]
pub(crate) fn declare_for_test(owner: &AgentId, names: &[&str]) {
    let list: Vec<Value> = names
        .iter()
        .map(|n| json!({"name": n, "inputSchema": {"type": "object"}}))
        .collect();
    declare_for_test_result(owner, &Value::Array(list)).expect("declared");
}

/// The registry's own verdict on a declaration, with no job to host the program.
#[cfg(test)]
pub(crate) fn declare_for_test_result(owner: &AgentId, list: &Value) -> Result<(), Value> {
    REGISTRY.declare(owner.as_str(), list, &RESERVED)
}
