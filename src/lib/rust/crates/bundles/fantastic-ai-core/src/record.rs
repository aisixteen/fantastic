//! THE RECORD an ai_core agent takes, declared once as a struct (issue #642): the
//! fields EVERY ai_core agent carries, whatever its backend, and the shared pieces a
//! backend's own record is built from (`#[serde(flatten)] ai: AiRecord`). The one
//! engine ([`fantastic_kernel::verbs::check_record`]) judges the fields' names and
//! types at `create_agent` / `update_agent`; [`validate_meta`] holds only what a type
//! cannot say (a blank text, the caching modes a backend cannot do).

use fantastic_bundle::py_strip;
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
    #[schemars(length(min = 1))]
    pub store: String,
    /// The ref in that store.
    #[schemars(length(min = 1))]
    #[serde(rename = "ref")]
    pub key_ref: String,
}

/// The fields EVERY ai_core agent takes, whatever its backend.
#[derive(Debug, JsonSchema)]
pub struct AiRecord {
    /// Tokens the model sees per turn.
    #[schemars(range(min = 1))]
    pub context_window: Option<i64>,
    /// Tokens of the window held back for the model's reply.
    #[schemars(range(min = 0))]
    pub output_reserve: Option<i64>,
    /// How the conversation is fitted into the window.
    pub context_strategy: Option<ContextStrategy>,
    /// Recent turns a compaction keeps verbatim.
    #[schemars(range(min = 1, max = 50))]
    pub recent_n: Option<i64>,
    /// How the conversation is kept in the model's view; `provider_cache` is refused by a backend that cannot cache.
    pub caching: Option<CachingMode>,
    /// A row id: the row that ends the stable prefix; it and every row before it are pinned.
    pub cache_primer_until: Option<i64>,
    /// How many times a turn whose link dropped is retried before it fails, with a 2 s, 4 s, 8 s backoff; 0 = off.
    #[schemars(range(min = 0, max = 5))]
    pub transport_retries: Option<i64>,
    /// Answers in a row that ran nothing before the chain gives up.
    #[schemars(range(min = 2, max = 50))]
    pub misfire_limit: Option<i64>,
    /// A system block that REPLACES the default one on every turn; null = the default.
    #[schemars(length(min = 1))]
    pub system_prompt: Option<String>,
    /// The file_bridge this agent reads files THROUGH; null = none.
    pub file_bridge_id: Option<String>,
    /// A path on `file_bridge_id`, read through it as the system block; null = none.
    pub system_prompt_file: Option<String>,
    /// Text placed BEFORE the system block on every turn; null = nothing.
    #[schemars(length(min = 1))]
    pub pre_system_prompt: Option<String>,
    /// Text placed AFTER the system block on every turn; null = nothing.
    #[schemars(length(min = 1))]
    pub post_system_prompt: Option<String>,
}

/// The refusal for a `pre_system_prompt` / `post_system_prompt` the record cannot
/// use.
#[must_use]
pub fn wrap_prompt_refusal(name: &str) -> String {
    format!(
        "{name} must be a non-empty string (it wraps the system block on every turn) — \
         null clears it"
    )
}

/// The refusal for a `system_prompt` the record cannot use.
pub const SYSTEM_PROMPT_REFUSAL: &str = "system_prompt must be a non-empty string (it \
     replaces the default system block on every turn) — null clears it";

/// The record's `pre_system_prompt` / `post_system_prompt` — a non-blank string,
/// else `None` (nothing is added).
#[must_use]
pub fn record_wrap_prompt(meta: &Map<String, Value>, name: &str) -> Option<String> {
    meta.get(name)
        .and_then(Value::as_str)
        .filter(|s| !py_strip(s).is_empty())
        .map(str::to_string)
}

/// The record's own `system_prompt` — a non-blank string, else `None` (the default
/// block). Precedence at a turn: the per-call argument > this > the default block.
#[must_use]
pub fn record_system_prompt(meta: &Map<String, Value>) -> Option<String> {
    meta.get("system_prompt")
        .and_then(Value::as_str)
        .filter(|s| !py_strip(s).is_empty())
        .map(str::to_string)
}

/// What a type cannot say about the fields every ai_core agent takes: a blank
/// system text, the prompt-file pairing.
#[must_use]
pub fn validate_meta(meta: &Map<String, Value>) -> Option<String> {
    if let Some(Value::String(s)) = meta.get("system_prompt") {
        if py_strip(s).is_empty() {
            return Some(SYSTEM_PROMPT_REFUSAL.to_string());
        }
    }
    for name in ["pre_system_prompt", "post_system_prompt"] {
        if let Some(Value::String(s)) = meta.get(name) {
            if py_strip(s).is_empty() {
                return Some(wrap_prompt_refusal(name));
            }
        }
    }
    crate::prompt_file::shape_error(meta)
}

/// What a provider backend's record carries beside the fields of its own server: the
/// models it lists and what they cost. Flattened into each backend's record.
#[derive(Debug, JsonSchema)]
pub struct ProviderRecord {
    /// The names this provider lists in `models` (strings, or objects with an `id` or `name`).
    pub models: Option<Vec<Value>>,
    /// What each model costs per token, by model name.
    pub prices: Option<Value>,
    /// The fields every ai_core agent takes.
    #[serde(flatten)]
    pub ai: AiRecord,
}

/// The schema of a positive number of seconds, or none.
pub fn positive_seconds(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["number", "null"],
        "exclusiveMinimum": 0,
    }))
    .expect("a number schema")
}
