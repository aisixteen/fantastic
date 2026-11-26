//! THE RECORD an ai_core chat takes, declared once as a struct (issue #642): the
//! fields EVERY chat carries ([`AiRecord`]), the request knobs it sends with each
//! request ([`RequestKnobs`], #632), and the shared pieces a provider backend's own
//! record is built from. The one engine ([`fantastic_kernel::verbs::check_record`])
//! judges the fields' names, types, ranges and patterns at `create_agent` /
//! `update_agent`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::constants::DEFAULT_THINKING_BUDGET;

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

/// How the conversation is kept in the model's view. Absent: `provider_cache`.
#[derive(Debug, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CachingMode {
    /// Every stored row, in order: no autofold, no cache marks.
    None,
    /// Every stored row, in order, autofolded, with the provider's cache marks where the backend places them.
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
    Some(1024)
}

fn default_recent_n() -> Option<i64> {
    Some(6)
}

fn default_transport_retries() -> Option<i64> {
    Some(0)
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
    /// A system block that REPLACES the default one on every turn; null = the default.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub system_prompt: Option<String>,
    /// The file_bridge this agent reads files THROUGH; null = none.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub file_bridge_id: Option<String>,
    /// The path of the file view `file_bridge_id` names, read through that view as the system block; null = none.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub system_prompt_file: Option<String>,
    /// Text placed BEFORE the system block on every turn; null = nothing.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub pre_system_prompt: Option<String>,
    /// Text placed AFTER the system block on every turn; null = nothing.
    #[schemars(length(min = 1), regex(pattern = r"\S"))]
    pub post_system_prompt: Option<String>,
    /// Bytes of one image, lowering what the model takes; null = the model's.
    #[schemars(range(min = 1))]
    pub image_max_bytes: Option<i64>,
    /// Pixels of one image, lowering what the model takes; null = the model's.
    #[schemars(range(min = 1))]
    pub image_max_pixels: Option<i64>,
    /// What the chat's provider says its model can do; written by the chat itself.
    pub provider_capabilities: Option<ProviderCapabilities>,
}

fn default_image_bytes() -> Option<i64> {
    Some(5_000_000)
}

fn default_image_pixels() -> Option<i64> {
    Some(1_150_000)
}

/// What a provider says one of its models takes of images.
#[derive(Debug, JsonSchema)]
pub struct ImageCapabilities {
    /// Does the model take images?
    pub images: Option<bool>,
    /// How many images one request may carry; null = no bound.
    #[schemars(range(min = 1))]
    pub max_images: Option<i64>,
    /// The bytes of one image.
    #[schemars(range(min = 1), default = "default_image_bytes")]
    pub max_bytes: Option<i64>,
    /// The pixels of one image.
    #[schemars(range(min = 1), default = "default_image_pixels")]
    pub max_pixels: Option<i64>,
}

/// The chat's `provider_capabilities`: the provider and model asked, and the model's entry.
#[derive(Debug, JsonSchema)]
pub struct ProviderCapabilities {
    /// The provider asked.
    pub provider: Option<String>,
    /// The model asked about.
    pub model: Option<String>,
    /// The model's entry (empty when it has none).
    pub capabilities: Option<ImageCapabilities>,
}

/// The declaration of a model's image capabilities, for a reader of one entry
/// ([`fantastic_kernel::RecordDecl::get`]: the entry's value, else the declared default).
pub mod image_caps {
    fantastic_kernel::record!(ImageCapabilities);
    use super::ImageCapabilities;
}

/// The declaration of the fields every ai_core agent takes, for a reader of one of them
/// ([`fantastic_kernel::RecordDecl::get`]: the record's value, else the declared default).
pub mod ai {
    fantastic_kernel::record!(AiRecord);
    use super::AiRecord;
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
/// models it lists, what they cost and the window it serves. Flattened into each
/// backend's record.
#[derive(Debug, JsonSchema)]
pub struct ProviderRecord {
    /// The names this provider lists in `models`.
    pub models: Option<Vec<String>>,
    /// What each model costs, by model name; a model with no entry is unpriced (never $0).
    pub prices: Option<std::collections::BTreeMap<String, ModelPrice>>,
    /// The model's window (tokens).
    #[schemars(range(min = 1))]
    pub context_window: Option<i64>,
}

/// How much a reasoning model wants to think: a chat-template input (a soft steer).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// Extra high: needs a thinking_token_budget.
    Xhigh,
}

/// THE REQUEST KNOBS (#632): what a CHAT sends with each request. They change the
/// request body, never the provider: chats with different knobs share one provider.
/// A knob the chat leaves out is the backend's own default; a backend whose server
/// takes no such knob does not send it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RequestKnobs {
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-k sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i64>,
    /// Min-p sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f64>,
    /// How much the model wants to think (xhigh needs a thinking_token_budget).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The hard cap on thinking tokens per pass, below max_tokens; absent, 8192 (half of max_tokens when that is lower); null, no cap.
    #[schemars(range(min = 1))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_token_budget: Option<i64>,
    /// Presence penalty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Frequency penalty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Repetition penalty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repetition_penalty: Option<f64>,
    /// No n-gram of this size repeats.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_repeat_ngram_size: Option<i64>,
    /// Sampling seed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    /// A turn's output budget (tokens); unset, the backend's default for its server.
    #[schemars(range(min = 1))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
}

/// The names of the [`RequestKnobs`], in declaration order.
pub const KNOBS: [&str; 13] = [
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "reasoning_effort",
    "thinking_token_budget",
    "presence_penalty",
    "frequency_penalty",
    "repetition_penalty",
    "no_repeat_ngram_size",
    "seed",
    "stop",
    "max_tokens",
];

/// The knobs a chat's record sets (a null is unset), as a `complete` carries them, with
/// THE THINKING CAP (#782) every pass carries: the record's `thinking_token_budget`; when
/// the record names none, [`DEFAULT_THINKING_BUDGET`] — half the chat's `max_tokens` when
/// that is lower, so the cap can fire; a `null` on the record is no cap. One field holds
/// all three: absent (the default), a number (the chat's), null (none).
#[must_use]
pub fn knobs_of(meta: &Map<String, Value>) -> Map<String, Value> {
    let mut knobs: Map<String, Value> = KNOBS
        .iter()
        .filter_map(|k| meta.get(*k).filter(|v| !v.is_null()).map(|v| ((*k).to_string(), v.clone())))
        .collect();
    if !meta.contains_key("thinking_token_budget") {
        let fitted = match meta.get("max_tokens").and_then(Value::as_i64) {
            Some(m) => DEFAULT_THINKING_BUDGET.min(m.div_euclid(2)),
            None => DEFAULT_THINKING_BUDGET,
        };
        if fitted >= 1 {
            knobs.insert("thinking_token_budget".into(), json!(fitted));
        }
    }
    knobs
}

/// A write a chat's knobs cannot honour, on the MERGED record: a thinking budget not
/// below the output budget could never fire, and a turn would end mid-thought.
#[must_use]
pub fn knobs_refusal(meta: &Map<String, Value>, record: &Map<String, Value>) -> Option<String> {
    let pick = |k: &str| meta.get(k).or_else(|| record.get(k)).and_then(Value::as_i64);
    match (pick("thinking_token_budget"), pick("max_tokens")) {
        (Some(b), Some(m)) if b >= m => Some(format!(
            "thinking_token_budget={b} is not below max_tokens={m} — it could \
             never fire, and a turn would end mid-thought with no answer; keep it \
             below max_tokens (half is a good start)"
        )),
        _ => None,
    }
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

fn default_stream_idle() -> Option<f64> {
    Some(60.0)
}

/// The read clock of a provider's answer stream, flattened into each provider backend's record.
#[derive(Debug, JsonSchema)]
pub struct StreamClock {
    /// The read clock on the answer stream: seconds with no byte from the server (headers included) before the turn is judged stalled and ends with an error row. A bound BETWEEN bytes, never a widened send ceiling.
    #[schemars(range(min = 0.001), default = "default_stream_idle")]
    pub stream_idle_seconds: f64,
}

/// The declaration of the stream clock, for a reader of it
/// ([`fantastic_kernel::RecordDecl::get`]: the record's value, else the declared default).
pub mod clock {
    fantastic_kernel::record!(StreamClock);
    use super::StreamClock;
}
