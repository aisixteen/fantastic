//! WHAT THE KERNEL DECLARES ITSELF (issue #642): its own verbs, the verbs every agent
//! answers, and the record fields every agent carries. The same declarations as a
//! bundle's ([`crate::verbs`]) — the kernel answers its own verbs through the same shared entry as any agent,
//! and `reflect` / `docs` render them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A task or execution named by its id, or by the name a caller gave its own execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IdOrName {
    /// The numeric id.
    Id(i64),
    /// The name the caller gave its own execution (text is never read as a number).
    Name(String),
}

impl From<i64> for IdOrName {
    fn from(n: i64) -> Self {
        Self::Id(n)
    }
}

impl From<String> for IdOrName {
    fn from(s: String) -> Self {
        Self::Name(s)
    }
}

impl From<&str> for IdOrName {
    fn from(s: &str) -> Self {
        Self::Name(s.to_string())
    }
}

impl IdOrName {
    /// The id as JSON: a number or a string.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Id(n) => Value::from(*n),
            Self::Name(s) => Value::from(s.as_str()),
        }
    }
}

/// The call a wire leg's hand-in carries onward: the `item` of its `inbound_call`
/// message, read once here by the kernel (the drain gate) and by the io-bridge.
#[derive(Debug, Clone, Deserialize)]
pub struct InboundItem {
    /// Who the onward call is for.
    pub target: String,
    /// The onward call's payload.
    pub payload: Value,
    /// Secret marks: dotted paths into `payload`.
    #[serde(default)]
    pub secret: Vec<String>,
}

/// A hand-in to a wire leg: the transport gives its leg an admitted frame as one
/// `inbound_call`, which the leg sends onward as itself; every other hand-in verb
/// is `Other`. An `inbound_call` that is not of this shape does not parse.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum HandIn {
    /// One admitted call.
    #[serde(rename = "inbound_call")]
    InboundCall {
        /// The call carried.
        item: InboundItem,
    },
    /// Any other verb handed to the leg.
    #[serde(other)]
    Other,
}

/// The schema of a positive number of seconds, or none.
fn positive_seconds(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["number", "null"],
        "exclusiveMinimum": 0,
    }))
    .expect("a number schema")
}

/// The tier of a `docs` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// One line per verb (the default).
    Oneliners,
    /// The whole text of each verb.
    Detailed,
}

impl Tier {
    /// The tier as a call spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Oneliners => "oneliners",
            Tier::Detailed => "detailed",
        }
    }
}

/// How much wiring a `reflect` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Detail {
    /// The roster (the default).
    Distilled,
    /// The wiring detail: the full tree.
    Full,
}

/// Which tree a `reflect` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TreeKind {
    /// The nested tree (the default).
    All,
    /// A flat index of ids.
    Ids,
    /// No tree.
    None,
}

/// Which installable bundles a `reflect` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BundlesKind {
    /// Each with its record schema.
    All,
    /// Their names.
    Ids,
}

/// THE KERNEL'S OWN VERBS, answered by the ROOT alone (address `kernel`): they act on
/// the TREE and the kernel-wide tables, so the agent a caller names is the operand,
/// not the actor.
#[allow(missing_docs)]
pub mod kernel {
    use super::*;

    /// Creates a CHILD OF `parent_id` — any agent you name (your own id grows your own subtree); 'kernel_state' for top-level. No default parent. A handler_module that is a meta bundle (`bundles/<name>/bundle.yaml` with a base and no program) runs its base, with the `record` of the bundle under your fields; The handler must be installed. Every argument beyond these is a field of that bundle's record (`reflect bundles=all` lists them), and a field it does not have is refused.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct CreateAgent {
        /// The bundle the agent runs (`file_bridge.tools`, or a bare bundle name such as `file_bridge`).
        #[schemars(length(min = 1))]
        pub handler_module: String,
        /// The agent that will OWN the new one: your own id for a tool you keep, `kernel_state` for a top-level agent.
        #[schemars(length(min = 1))]
        pub parent_id: String,
        /// Its id (one safe path segment: no separator, no `|`, not hidden, no `..`, not all digits). Absent: minted from the bundle's name.
        #[schemars(regex(pattern = "^[0-9]*[^/\\\\|.0-9](?:[^/\\\\|.]|\\.[^/\\\\|.])*\\.?$"))]
        pub id: Option<String>,
        /// The fields of the new agent's record — whatever its bundle's record declares.
        #[serde(flatten)]
        pub fields: Map<String, Value>,
    }

    /// EVERY FIELD YOU NAME IS REPLACED, NOT MERGED (a field you do not name is untouched), so a dict field must be sent WHOLE — `can_call`/`callable_by` included: restate every key you want to keep, the baseline `$self` among them, or the keys you omit are GONE. Granting one edge reads like a PATCH and behaves like a PUT. Whoever holds update_agent on the root updates any agent, itself included. Every argument beyond `id` is a field of the agent's bundle's record, and a field it does not have is refused.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct UpdateAgent {
        /// The agent to update.
        #[schemars(length(min = 1))]
        pub id: String,
        /// The fields to replace in its record — whatever its bundle's record declares.
        #[serde(flatten)]
        pub fields: Map<String, Value>,
    }

    /// CASCADES — deletes that agent AND ITS WHOLE SUBTREE, deepest first; refuses if any descendant carries `delete_lock`. Whoever holds delete_agent on the root deletes any agent.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct DeleteAgent {
        /// The agent to delete.
        #[schemars(length(min = 1))]
        pub id: String,
        /// `true` leaves the persisted memory of the whole subtree alone.
        pub keep_memory: Option<bool>,
    }

    /// Every agent's record, flat. Paged.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ListAgents {
        /// A page size: absent, everything.
        #[schemars(range(min = 1))]
        pub limit: Option<i64>,
        /// The cursor `next` of the previous page.
        pub after: Option<String>,
    }

    /// The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, verb, started_at}]}, one row per in-flight dispatch. Paged.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Ps {
        /// A page size: absent, everything.
        #[schemars(range(min = 1))]
        pub limit: Option<i64>,
        /// The cursor `next` of the previous page.
        pub after: Option<String>,
    }

    /// EVERY execution and what it became — named under its id, unnamed under its minted pid — {executions:[{id, target, status, attempts, retries, named, outcome?, acked?}]}, paged, scoped to what YOU originated; read from the kernel's memory store plus what is not yet flushed. `outcome` keeps the RESULT only for a named call; a dead letter keeps its error; anything else is {status, bytes} — the reply itself went to its caller. Failed/timed_out/cancelled rows are the dead-letter queue (announced by the `on_deadletter` event): inspect, retry with a NEW id if you choose, then `ack_deadletter` it — nothing is retried silently.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Ledger {
        /// A page size: absent, everything.
        #[schemars(range(min = 1))]
        pub limit: Option<i64>,
        /// The cursor `next` of the previous page.
        pub after: Option<String>,
    }

    /// Acknowledges a DEAD LETTER (failed/timed_out/cancelled) you originated: recorded in the kernel's store, it may then be pruned. Returns {acked:true, id}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AckDeadletter {
        /// An execution id, or its name.
        pub id: IdOrName,
    }

    /// THE START SEQUENCE — the root must name `memory_store_id` (fail loud otherwise): boots the root, then that store, starts flushing the kernel's execution rows into it, then boots every other agent in registration order. Idempotent. Returns {started, store, booted:[ids]}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct KernelStart {}

    /// Kills that task AND ITS SUBTREE — the id names its own agent. Returns {cancelled:bool, killed:[…]}; the blocked caller's batch item settles as status:'cancelled'. Unknown id ⇒ {cancelled:false}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Cancel {
        /// The task's id (`ps` lists them), or the name the caller gave its own execution.
        pub task_id: IdOrName,
    }

    /// Stops the AGENTS, not the process: drains (new roots refused), sends `shutdown` to every agent advertising it, waits for in-flight work, cancels the rest. Returns {drained, shut:[ids], cancelled:[ids]}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ShutdownAgents {}

    /// Stops the kernel PROCESS gracefully — drain, every call settles (done, or cancelled into the dead letters), the last rows flush, agents shut down, exit. Under env:'container', the container.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct KernelStop {}

    /// HOW TO CALL an agent — its bundle's verbs: `oneliners` (default) is one line per verb (its arguments and what it does), `detailed` the full text with every argument. Without `verb` the reply also carries the bundle's short `man` text. Pull once per bundle; `detailed` for a verb you are about to use. Gated like every kernel verb: a `can_call` grant of `docs` on the root. Returns {of, bundle, tier, verbs, man?}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Docs {
        /// An agent id (`<guid>|<agent id>` on a connected kernel), a bundle, or 'kernel'.
        #[schemars(regex(pattern = "^[^|]+(?:\\|[^|]+)?$"))]
        pub of: String,
        /// How much of each verb: `oneliners` (default) or `detailed`.
        pub tier: Option<Tier>,
        /// One verb's entry only.
        pub verb: Option<String>,
    }

    /// Waits for EVERY named agent to send YOU a message of type 'report' (its `state` and `result`); a report from a named agent is HELD for the one combined message, not delivered as its own turn. Returns at once {wait_id, label, ids, timeout_s}; later ONE message reaches you — {wait_id, label, done:[{id, state, result}], missing:[ids], timed_out} — when all have reported or `timeout_s` passes. Refuses a missing or empty `ids`, an id that names no agent (unknown_agent), yourself, a non-positive `timeout_s` and a non-string `label`. The wait is a row: it survives a restart.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AwaitSet {
        /// The agent ids to wait for.
        #[schemars(length(min = 1))]
        pub ids: Vec<String>,
        /// A positive number of seconds after which the wait ends as timed out.
        #[schemars(schema_with = "positive_seconds")]
        #[serde(default)]
        pub timeout_s: Option<f64>,
        /// A name for the wait, echoed in the combined message.
        pub label: Option<String>,
    }

    /// Drops YOUR wait; the reports it held are then delivered one by one, each as its own message. Returns {cancelled:bool, wait_id, released:int}. A wait that is unknown or not yours ⇒ {cancelled:false}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AwaitCancel {
        /// The `wait_id` `await_set` returned.
        pub wait_id: i64,
    }

    crate::verbs! {
        @own
        "create_agent" => CreateAgent,
        "update_agent" => UpdateAgent,
        "delete_agent" => DeleteAgent,
        "list_agents" => ListAgents,
        "ps" => Ps,
        "ledger" => Ledger,
        "ack_deadletter" => AckDeadletter,
        "kernel_start" => KernelStart,
        "cancel" => Cancel,
        "shutdown_agents" => ShutdownAgents,
        "kernel_stop" => KernelStop,
        "docs" => Docs,
        "await_set" => AwaitSet,
        "await_cancel" => AwaitCancel,
    }
}

/// THE VERBS EVERY AGENT ANSWERS, declared once and added to each agent's own.
#[allow(missing_docs)]
pub mod common {
    use super::*;

    /// What this agent IS: identity, sentence, state, grants — and, by flags, its subtree and the installable bundles. How to CALL it is `docs`.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Reflect {
        /// `full` adds the wiring detail (the full tree).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub detail: Option<Detail>,
        /// `all` (default) the nested tree, `ids` a flat id index, `none` omits it.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tree: Option<TreeKind>,
        /// Answer with only these branches (agent ids in this agent's tree).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub show_subtrees: Option<Vec<String>>,
        /// Prune these branches to a stub `{id, pruned}`.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub ignore_subtrees: Option<Vec<String>>,
        /// List only the agents running one of these handler names (or a role on it).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub handler_modules: Option<Vec<String>>,
        /// The installable bundles: `all` with their record schemas, `ids` the names.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub bundles: Option<BundlesKind>,
        /// A page size for `tree=ids` and `bundles`.
        #[schemars(range(min = 1))]
        #[serde(skip_serializing_if = "Option::is_none")]
        pub limit: Option<i64>,
        /// The cursor of the previous page.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub after: Option<String>,
    }

    crate::verbs! {
        @own
        "reflect" => Reflect,
    }
}

/// THE FIELDS EVERY RECORD CARRIES — read by the kernel itself.
#[allow(missing_docs)]
pub mod record {
    use super::*;

    /// The fields the kernel reads on any agent's record.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct CommonRecord {
        /// The name a view draws.
        pub display_name: Option<String>,
        /// One line saying what the agent is for; every reflect shows it.
        pub description: Option<String>,
        /// What this agent may call: a map of target → verbs (`*` for all). Sent WHOLE.
        #[serde(default)]
        #[schemars(schema_with = "grant_map")]
        pub can_call: Option<Value>,
        /// Who may call this agent: a map of caller → verbs (`*` for all). Sent WHOLE.
        #[serde(default)]
        #[schemars(schema_with = "grant_map")]
        pub callable_by: Option<Value>,
        /// `true` refuses a delete of this agent or an ancestor.
        pub delete_lock: Option<bool>,
        /// The folder name its children live under on disk (one safe segment: no separator, no `|`, not hidden, no `..`, not all digits).
        #[serde(default = "agents_dir")]
        #[schemars(regex(pattern = "^[0-9]*[^/\\\\|.0-9](?:[^/\\\\|.]|\\.[^/\\\\|.])*\\.?$"))]
        pub children_dir: String,
        /// How a client draws the agent.
        pub view: Option<Value>,
        /// The agent that stores the kernel's execution rows (the root's).
        pub memory_store_id: Option<String>,
    }

    /// The schema of a grant map: each key `"*"` (every verb) or a list of verbs.
    fn grant_map(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        serde_json::from_value(serde_json::json!({
            "type": ["object", "null"],
            "additionalProperties": {
                "anyOf": [
                    {"enum": ["*"]},
                    {"type": "array", "items": {"type": "string"}}
                ]
            }
        }))
        .expect("a grant map schema")
    }

    fn agents_dir() -> String {
        "agents".to_string()
    }

    /// The folder name the children of an agent with this record live under: its own
    /// `children_dir`, else the declared default.
    #[must_use]
    pub fn children_dir(meta: &serde_json::Map<String, serde_json::Value>) -> String {
        // the record's own text is kept as written (a malformed one is refused where the
        // tree is walked, never replaced by the default); only an absent one takes the default
        match meta.get("children_dir") {
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => RECORD
                .get(&serde_json::Map::new(), "children_dir")
                .and_then(|v| v.as_str().map(str::to_string))
                .expect("children_dir declares a default"),
        }
    }

    crate::record!(CommonRecord);
}

/// THE RECORD OF A BARE AGENT (no `handler_module`): the kernel is its agent, and a
/// bare agent carries whatever fields it was given — an open record.
#[allow(missing_docs)]
pub mod bare_record {
    use super::*;

    /// A bare agent's own fields: none declared, all accepted.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct BareRecord {}

    crate::record!(BareRecord, open);

    /// Judge the fields of an `update_agent` patch on a bare agent: the common record,
    /// open to whatever fields it was given. The agent-side entry for a record with no bundle.
    pub fn check(
        verb: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), serde_json::Value> {
        crate::verbs::check_record(
            verb,
            "bare agent",
            fields,
            &crate::declared::record::RECORD,
            &RECORD,
        )
    }
}

/// The kernel's own verbs, as `name → declaration`, for `docs of=kernel`.
#[must_use]
pub fn kernel_verbs() -> &'static [crate::verbs::Verb] {
    kernel::VERBS
}
