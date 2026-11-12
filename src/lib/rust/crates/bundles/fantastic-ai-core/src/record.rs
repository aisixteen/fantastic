//! THE RECORD an ai_core agent takes, declared once as a struct (issue #642): the
//! fields EVERY ai_core agent carries, whatever its backend, and the shared pieces a
//! backend's own record is built from (`#[serde(flatten)] ai: AiRecord`). The one
//! engine ([`fantastic_kernel::verbs::check_record`]) judges the fields' names, types,
//! ranges and patterns at `create_agent` / `update_agent`.

use schemars::JsonSchema;
use serde_json::{Map, Value};

/// How the conversation is fitted into the window.
#[derive(Debug, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ContextStrategy {
    /// Summarise what falls out of the window.
    Compact,
    /// Drop what falls out of the window.
    Truncate,
    /// Strip the bulky parts of old turns.
    Strip,
    /// Fold the oldest spans into fold rows.
    Fold,
    /// Never shrink: an over-budget turn is refused.
    None,
}

/// How the conversation is kept in the model's view, for a backend that cannot place
/// the provider's cache markers.
#[derive(Debug, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UncachedMode {
    /// Every stored row, in order, never trimmed.
    None,
    /// Trim superseded reads, repeated reply bodies and used docs from the view.
    Cleanup,
}

/// How the conversation is kept in the model's view.
#[derive(Debug, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CachingMode {
    /// Every stored row, in order, never trimmed.
    None,
    /// Trim superseded reads, repeated reply bodies and used docs from the view.
    Cleanup,
    /// None's history plus the provider's cache markers, with autofold.
    ProviderCache,
}

/// Where a key is read from: the credential store and the ref holding `api_key`.
#[derive(Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// The credential store agent.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub store: String,
    /// The ref in that store.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    #[serde(rename = "ref")]
    pub key_ref: String,
}

fn default_output_reserve() -> Option<i64> {
    Some(crate::constants::DEFAULT_OUTPUT_RESERVE)
}

fn default_recent_n() -> Option<i64> {
    Some(crate::constants::RECENT_N)
}

fn default_transport_retries() -> Option<i64> {
    Some(crate::constants::TRANSPORT_RETRIES)
}

fn default_misfire_limit() -> Option<i64> {
    Some(crate::constants::MAX_SILENT_TURNS as i64)
}

/// The fields EVERY ai_core agent takes, whatever its backend.
#[derive(Debug, JsonSchema)]
pub struct AiRecord {
    /// Tokens the model sees per turn.
    #[schemars(range(min = 1))]
    pub context_window: Option<i64>,
    /// Tokens of the window held back for the model's reply.
    #[schemars(range(min = 0), default = "default_output_reserve")]
    pub output_reserve: Option<i64>,
    /// How the conversation is fitted into the window.
    pub context_strategy: Option<ContextStrategy>,
    /// Recent turns a compaction keeps verbatim.
    #[schemars(range(min = 1, max = 50), default = "default_recent_n")]
    pub recent_n: Option<i64>,
    /// A row id: the row that ends the stable prefix; it and every row before it are pinned.
    #[schemars(range(min = 1))]
    pub cache_primer_until: Option<i64>,
    /// How many times a turn whose link dropped is retried before it fails, with a 2 s, 4 s, 8 s backoff; 0 = off.
    #[schemars(range(min = 0, max = 5), default = "default_transport_retries")]
    pub transport_retries: Option<i64>,
    /// Answers in a row that ran nothing before the chain gives up.
    #[schemars(range(min = 2, max = 50), default = "default_misfire_limit")]
    pub misfire_limit: Option<i64>,
    /// A system block that REPLACES the default one on every turn; null = the default.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub system_prompt: Option<String>,
    /// The file_bridge this agent reads files THROUGH; null = none.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub file_bridge_id: Option<String>,
    /// A path on `file_bridge_id`, read through it as the system block; null = none.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub system_prompt_file: Option<String>,
    /// Text placed BEFORE the system block on every turn; null = nothing.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub pre_system_prompt: Option<String>,
    /// Text placed AFTER the system block on every turn; null = nothing.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub post_system_prompt: Option<String>,
}

/// The record's `pre_system_prompt` / `post_system_prompt`, as declared (text with a
/// non-space character, or absent).
#[must_use]
pub fn record_wrap_prompt(meta: &Map<String, Value>, name: &str) -> Option<String> {
    meta.get(name).and_then(Value::as_str).map(str::to_string)
}

/// The record's own `system_prompt`, as declared (text with a non-space character, or
/// absent). Precedence at a turn: the per-call argument > this > the default block.
#[must_use]
pub fn record_system_prompt(meta: &Map<String, Value>) -> Option<String> {
    meta.get("system_prompt")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// What one model costs, USD per million tokens.
#[derive(Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    /// Per million input tokens.
    #[schemars(range(min = 0))]
    pub input: f64,
    /// Per million output tokens.
    #[schemars(range(min = 0))]
    pub output: f64,
    /// Per million cache-read input tokens.
    #[schemars(range(min = 0))]
    pub cache_read: Option<f64>,
    /// Per million 5-minute cache-write input tokens.
    #[schemars(range(min = 0))]
    pub cache_write_5m: Option<f64>,
}

/// What a provider backend's record carries beside the fields of its own server: the
/// models it lists and what they cost. Flattened into each backend's record.
#[derive(Debug, JsonSchema)]
pub struct ProviderRecord {
    /// The names this provider lists in `models`.
    pub models: Option<Vec<String>>,
    /// What each model costs, by model name; a model with no entry is unpriced (never $0).
    pub prices: Option<std::collections::BTreeMap<String, ModelPrice>>,
    /// The fields every ai_core agent takes.
    #[serde(flatten)]
    pub ai: AiRecord,
}

/// The schema of a positive number of seconds, or none, with its declared `default`.
#[must_use]
pub fn positive_seconds_of(default: f64) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["number", "null"],
        "exclusiveMinimum": 0,
        "default": default,
    }))
    .expect("a number schema")
}

/// The schema of a positive number of seconds, or none, whose default another field names.
pub fn positive_seconds(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["number", "null"],
        "exclusiveMinimum": 0,
    }))
    .expect("a number schema")
}

/// The schema of the record's `stream_idle_seconds`: a positive number of seconds, or
/// none (the default is [`crate::constants::STREAM_IDLE_SECONDS`]).
pub fn stream_idle_seconds(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    positive_seconds_of(crate::constants::STREAM_IDLE_SECONDS)
}
