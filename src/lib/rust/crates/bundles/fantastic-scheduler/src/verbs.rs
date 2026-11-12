//! The verbs a scheduler declares (issue #642): one struct per verb, then `verbs!` —
//! the list the agent's own entry (`Call::parse`) checks every call against, and
//! that `reflect` and `docs` render. `reflect` is declared once, in the kernel.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Start the tick loop; idempotent. Persistence is THROUGH the loader. Answers {running:true}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// Add a recurring call: every fire runs AS THE SCHEDULER, so it reaches what the scheduler's can_call grants. Persisted THROUGH the loader; refused when no store is wired. Returns {schedule_id, schedule}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// The agent each fire calls.
    #[schemars(length(min = 1))]
    pub target: String,
    /// The call itself: an object whose `type` is any verb `target` answers, with that verb's arguments.
    #[schemars(schema_with = "call_with_type")]
    pub payload: Map<String, Value>,
    /// Seconds between fires.
    #[serde(default = "sixty")]
    #[schemars(range(min = 1))]
    pub interval_seconds: u64,
    /// Seconds each fire may take; absent = wait forever. Not `timeout`, which is a call's own routing field.
    #[serde(default)]
    #[schemars(schema_with = "positive_seconds")]
    pub fire_timeout: Option<serde_json::Number>,
}

/// The shape every stored schedule has (what `schedule` writes): decoded strictly
/// from `schedules.yaml` before a fire; other stored fields are carried untouched.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Stored {
    /// The agent each fire calls.
    pub target: String,
    /// The stored call.
    pub payload: Map<String, Value>,
    /// Epoch seconds of the next fire.
    pub next_run: f64,
    /// Seconds between fires.
    pub interval_seconds: f64,
    /// Fires so far.
    pub run_count: u64,
}

fn sixty() -> u64 {
    60
}

/// The schema of a per-fire bound: a number greater than zero.
fn positive_seconds(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["number", "null"],
        "exclusiveMinimum": 0.0
    }))
    .expect("a seconds schema")
}

/// The schema of a schedule's call: an object whose `type` is text.
fn call_with_type(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "required": ["type"],
        "properties": {
            "type": {"type": "string", "minLength": 1, "description": "The verb the target answers."}
        }
    }))
    .expect("a call schema")
}

/// Remove one schedule. Returns {removed:bool, schedule_id}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Unschedule {
    /// The schedule to remove.
    #[schemars(length(min = 1))]
    pub schedule_id: String,
}

/// Every schedule. Returns {schedules:[<schedule record>, ...]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {}

/// Pause one schedule, or the whole scheduler when no id is given.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pause {
    /// The schedule to pause; absent, the whole scheduler.
    #[schemars(length(min = 1))]
    pub schedule_id: Option<String>,
}

/// Resume one schedule, or the whole scheduler when no id is given.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resume {
    /// The schedule to resume; absent, the whole scheduler.
    #[schemars(length(min = 1))]
    pub schedule_id: Option<String>,
}

/// Fire one schedule now, as the scheduler: bumps run_count, persists, then fires. Returns {fired:true, schedule_id, result, error} — the fire's outcome as `history` records it: the target's reply, or why the fire failed.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TickNow {
    /// The schedule to fire.
    #[schemars(length(min = 1))]
    pub schedule_id: String,
}

/// The newest fires of every schedule. Returns {history:[event, ...], count}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct History {
    /// How many of the newest fires, at most 500 (above 500 is clamped to 500).
    #[serde(default = "hundred")]
    #[schemars(range(min = 1))]
    pub limit: u64,
    /// Keep only this schedule's fires.
    #[schemars(length(min = 1))]
    pub schedule_id: Option<String>,
}

fn hundred() -> u64 {
    100
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "schedule" => Schedule,
    "unschedule" => Unschedule,
    "list" => List,
    "pause" => Pause,
    "resume" => Resume,
    "tick_now" => TickNow,
    "history" => History,
}
