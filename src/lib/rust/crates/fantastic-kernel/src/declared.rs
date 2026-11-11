//! WHAT THE KERNEL DECLARES ITSELF (issue #642): its own verbs, the verbs every agent
//! answers, and the record fields every agent carries. The same declarations as a
//! bundle's ([`crate::verbs`]) — the kernel answers its own verbs through the same shared entry as any agent,
//! and `reflect` / `docs` render them.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// A task or execution named by its id, or by the name a caller gave its own execution.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IdOrName {
    /// The numeric id.
    Id(i64),
    /// The decimal text of an id, or the name the caller gave.
    Name(String),
}

/// The tier of a `docs` answer.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// One line per verb (the default).
    Oneliners,
    /// The whole text of each verb.
    Detailed,
}

/// THE KERNEL'S OWN VERBS, answered by the ROOT alone (address `kernel`): they act on
/// the TREE and the kernel-wide tables, so the agent a caller names is the operand,
/// not the actor.
#[allow(missing_docs)]
pub mod kernel {
    use super::*;

    /// Creates a CHILD OF `parent_id` — any agent you name (your own id grows your own subtree); 'kernel_state' for top-level. No default parent. A handler_module that is a meta bundle (`bundles/<name>/bundle.yaml` with a base and no program) runs its base, with the `record` of the bundle under your fields; `bundle:<name>` beside the base is the same. The handler must be installed. Every argument beyond these is a field of that bundle's record (`reflect bundles=all` lists them), and a field it does not have is refused.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct CreateAgent {
        /// The bundle the agent runs (`file_bridge.tools`, or a bare bundle name such as `file_bridge`).
        pub handler_module: Option<String>,
        /// The agent that will OWN the new one: your own id for a tool you keep, `kernel_state` for a top-level agent.
        pub parent_id: String,
        /// Its id. Absent: minted from the bundle's name.
        pub id: Option<String>,
        /// The bundle a record inherits from when it runs another base than the bundle's own.
        pub bundle: Option<String>,
    }

    /// EVERY FIELD YOU NAME IS REPLACED, NOT MERGED (a field you do not name is untouched), so a dict field must be sent WHOLE — `can_call`/`callable_by` included: restate every key you want to keep, the baseline `$self` among them, or the keys you omit are GONE. Granting one edge reads like a PATCH and behaves like a PUT. Whoever holds update_agent on the root updates any agent, itself included. Every argument beyond `id` is a field of the agent's bundle's record, and a field it does not have is refused.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct UpdateAgent {
        /// The agent to update.
        pub id: String,
    }

    /// CASCADES — deletes that agent AND ITS WHOLE SUBTREE, deepest first; refuses if any descendant carries `delete_lock`. Whoever holds delete_agent on the root deletes any agent.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct DeleteAgent {
        /// The agent to delete.
        pub id: String,
        /// `true` leaves the persisted memory of the whole subtree alone.
        pub keep_memory: Option<bool>,
    }

    /// Every agent's record, flat. Paged.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ListAgents {
        /// A page size: absent, everything.
        pub limit: Option<i64>,
        /// The cursor `next` of the previous page.
        pub after: Option<String>,
    }

    /// The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, verb, started_at}]}, one row per in-flight dispatch. Paged.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Ps {
        /// A page size: absent, everything.
        pub limit: Option<i64>,
        /// The cursor `next` of the previous page.
        pub after: Option<String>,
    }

    /// EVERY execution and what it became — named under its id, unnamed under its minted pid — {executions:[{id, target, status, attempts, retries, named, outcome?, acked?}]}, paged, scoped to what YOU originated; read from the kernel's memory store plus what is not yet flushed. `outcome` keeps the RESULT only for a named call; a dead letter keeps its error; anything else is {status, bytes} — the reply itself went to its caller. Failed/timed_out/cancelled rows are the dead-letter queue (announced by the `on_deadletter` event): inspect, retry with a NEW id if you choose, then `ack_deadletter` it — nothing is retried silently.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Ledger {
        /// A page size: absent, everything.
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

    /// HOW TO CALL an agent — its bundle's verbs: `oneliners` (default) is one line per verb (its arguments and what it does), `detailed` the full text with every argument (no `verb`: plus emits, cancel and notes). Pull once per bundle; `detailed` for a verb you are about to use. Gated like every kernel verb: a `can_call` grant of `docs` on the root. Returns {of, bundle, tier, verbs, emits?, cancel?, notes?}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Docs {
        /// An agent id (`<guid>|<agent id>` on a connected kernel), a bundle, or 'kernel'.
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
        pub ids: Vec<String>,
        /// A positive number of seconds after which the wait ends as timed out.
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

    /// What this agent IS: identity, sentence, state, grants — and, by flags, its subtree, the installable bundles and its readme. How to CALL it is `docs`.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Reflect {
        /// `full` adds the wiring detail (the full tree, the manual index).
        pub detail: Option<String>,
        /// `all` (default) the nested tree, `ids` a flat id index; `none` or `false` omits it.
        pub tree: Option<Value>,
        /// Answer with only these branches (agent ids in this agent's tree).
        pub show_subtrees: Option<Vec<String>>,
        /// Prune these branches to a stub `{id, pruned}`.
        pub ignore_subtrees: Option<Vec<String>>,
        /// List only the agents running one of these handler names (or a role on it).
        pub handler_modules: Option<Vec<String>>,
        /// The installable bundles: `all` with their record schemas, `ids` the names.
        pub bundles: Option<String>,
        /// `true` the short orientation readme, `index` the manual's topics, a key one topic.
        pub readme: Option<Value>,
        /// A page size for `tree=ids` and `bundles`.
        pub limit: Option<i64>,
        /// The cursor of the previous page.
        pub after: Option<String>,
    }

    /// The kernel starts the agent: open what it holds (files, tasks). Sent by the kernel, no arguments.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Boot {}

    /// The kernel stops the agent: release what it holds. Sent by the kernel.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Shutdown {
        /// Why it stops (`reloaded` when its bundle was reloaded).
        pub reason: Option<String>,
    }

    crate::verbs! {
        @own
        "reflect" => Reflect,
        "boot" => Boot,
        "shutdown" => Shutdown,
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
        pub can_call: Option<Value>,
        /// Who may call this agent: a map of caller → verbs (`*` for all). Sent WHOLE.
        pub callable_by: Option<Value>,
        /// `true` refuses a delete of this agent or an ancestor.
        pub delete_lock: Option<Value>,
        /// The folder name its children live under on disk (one safe segment).
        pub children_dir: Option<String>,
        /// How a client draws the agent.
        pub view: Option<Value>,
        /// The agent that stores the kernel's execution rows (the root's).
        pub memory_store_id: Option<String>,
        /// The bundle the record inherits its fields from.
        pub bundle: Option<String>,
    }

    crate::record!(CommonRecord);
}

/// The kernel's own verbs, as `name → declaration`, for `docs of=kernel`.
#[must_use]
pub fn kernel_verbs() -> &'static [crate::verbs::Verb] {
    kernel::VERBS
}
