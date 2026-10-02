//! foundation_models_backend — an LLM agent on Apple's on-device Foundation
//! Models system model.
//!
//! Owner's decision (2026-10-02): this backend in BOTH swift and rust, with ONE
//! implementation of the FoundationModels calls — the swift `FantasticFMCore`.
//! The swift bundle calls it directly; this crate links it as a static library
//! through `FantasticFMBridge`'s C ABI (`build.rs`, [`system`]). Python has none
//! (Apple-only native), so parity is swift <-> rust: the same bundle name, record
//! schema, reflect shape and refusal texts, byte for byte, and ONE readme file
//! (`src/lib/swift/Sources/FantasticFoundationModelsBackend/Resources/readme.md`).
//!
//! A thin shell over `fantastic-ai-core`, like every backend: this crate supplies
//! the [`Provider`] (messages -> one FMCore request, raw text out) and the
//! `Bundle`; the threads, FIFO lock, prompt assembly, `<call>` parsing, context
//! management and history all live in ai-core.
//!
//! On-device: no endpoint, no model choice, no credentials — a record naming one
//! is refused at the write rather than shown in reflect as if it meant something.
//!
//! RAW tool-calling: the session is given no tools and no schema; the provider
//! streams raw text and ai-core owns the `<call>` grammar. Never FM's Tool
//! protocol, never guided generation.
//!
//! NO FALLBACKS: an unavailable model fails the turn LOUD, naming the reason
//! (device not eligible / Apple Intelligence not enabled / model not ready /
//! platform not macOS 26+) — checked FIRST in the turn ([`Provider::ready`]),
//! before the context is projected. Nothing is routed to another backend.
//!
//! THE WINDOW is the model's own, read from the SDK (`fm_context_size`): the
//! `context_window` default in the schema and in the context machinery
//! ([`BackendConfig::model_window`]), and a record may only name a smaller one.

#![deny(missing_docs)]

pub mod engine;
pub mod system;

use async_trait::async_trait;
use engine::{unavailable_message, FmEngine, FmEvent, FmRequest, FmStream, FmTurn};
use fantastic_ai_core::provider::{
    Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream, ReportCell,
};
use fantastic_ai_core::schema::{self, Field};
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, state, verbs};
use fantastic_bundle::{py_repr_value, py_str};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "foundation_models_backend.tools";

/// The bundle's name — the prefix of every refusal it raises.
pub const NAME: &str = "foundation_models_backend";

/// readme.md seeded into the agent's dir — THE file the swift bundle serves too.
pub const README: &str = include_str!(
    "../../../../../swift/Sources/FantasticFoundationModelsBackend/Resources/readme.md"
);

/// What `reflect` says this agent IS — swift's sentence, verbatim.
pub const SENTENCE: &str = "Apple Foundation Models LLM agent — the on-device system model (raw prompt-and-parse tool-calling; no endpoint, no model choice, no key).";

/// The fields an on-device record must NOT carry — there is nothing for them to
/// mean here, and reflect shows `model`/`endpoint` straight off the record.
pub const OFF_DEVICE_FIELDS: [&str; 3] = ["endpoint", "model", "credentials"];

/// The model id the provider reports — swift's `FoundationModelsProvider.model`.
pub const MODEL: &str = "SystemLanguageModel.default";

/// The on-device backend bundle over an engine — the system model in a kernel
/// ([`FoundationModelsBackendBundle::system`]), a fake in the unit suite.
pub struct FoundationModelsBackendBundle {
    engine: Arc<dyn FmEngine>,
}

impl FoundationModelsBackendBundle {
    /// The bundle over `engine`.
    #[must_use]
    pub fn new(engine: Arc<dyn FmEngine>) -> Self {
        Self { engine }
    }

    /// The bundle over the system model ([`system::SystemEngine`]). Touches
    /// nothing until it is used — registering it costs no model call.
    #[must_use]
    pub fn system() -> Self {
        Self::new(Arc::new(system::SystemEngine::new()))
    }

    /// The ai-core config: this bundle's name and the model's REAL window.
    fn cfg(&self) -> BackendConfig {
        BackendConfig {
            name: NAME,
            model_window: self.engine.context_size(),
        }
    }

    /// THE RECORD SCHEMA — its own two knobs, then the shared AI fields, with
    /// `context_window` defaulting to the model's real window. Neither knob has a
    /// default here: unset, the model uses its own. Swift
    /// `FoundationModelsBackendBundle.schema(contextWindow:)`.
    #[must_use]
    pub fn schema_for_window(context_window: i64) -> Value {
        schema::schema_with_window(
            vec![
                Field::new(
                    "temperature",
                    "float",
                    "sampling temperature (>= 0); unset, the on-device model's own",
                ),
                Field::new(
                    "max_tokens",
                    "int",
                    "a turn's output budget (tokens); unset, the on-device model's own",
                ),
            ],
            context_window,
        )
    }

    /// The provider for one pass. An UNAVAILABLE model makes a provider that
    /// refuses the turn up front ([`Provider::ready`]) — naming the reason,
    /// before the context is projected — so the caller reads why, not a window
    /// failure on a model that could never have answered.
    fn build_provider(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Arc<dyn Provider> {
        let meta = helpers::agent_meta(agent_id, kernel);
        let refusal = self
            .engine
            .availability()
            .reason()
            .map(|r| ProviderError::raised(format!("{NAME}: {}", unavailable_message(r))));
        Arc::new(FoundationModelsProvider {
            engine: Arc::clone(&self.engine),
            temperature: meta.get("temperature").and_then(number),
            max_tokens: meta.get("max_tokens").and_then(int),
            refusal,
            report: ReportCell::default(),
        })
    }

    /// `reflect` — the shared base, then (in this order) whether the model can
    /// answer and why not, its real window, and the two knobs as the record sets
    /// them (null = the model's own). Swift `reflectExtra`.
    fn reflect_reply(&self, agent_id: &AgentId, kernel: &Kernel) -> Value {
        let mut out = verbs::reflect_base(agent_id, kernel, SENTENCE, self.cfg().model_window);
        let meta = helpers::agent_meta(agent_id, kernel);
        let a = self.engine.availability();
        out.insert("available".to_string(), json!(a.is_available()));
        out.insert(
            "unavailable_reason".to_string(),
            a.reason().map_or(Value::Null, |r| json!(r)),
        );
        out.insert(
            "context_size".to_string(),
            json!(self.engine.context_size()),
        );
        for key in ["temperature", "max_tokens"] {
            out.insert(
                key.to_string(),
                meta.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        Value::Object(out)
    }
}

/// An int or a float as an `f64` — a bool is neither. Swift `number`.
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// A JSON INT — never a float, even an integral one (`5.0`), never a bool.
fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) if !n.is_f64() => n.as_i64(),
        _ => None,
    }
}

#[async_trait]
impl Bundle for FoundationModelsBackendBundle {
    fn name(&self) -> &str {
        NAME
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// Refuse, AT THE WRITE, a record this backend cannot honour — each check
    /// names the field and the legal shape; the schema's own check runs last.
    /// Swift `validateMeta`, check for check and word for word.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        record: &Map<String, Value>,
    ) -> Option<String> {
        let set = |key: &str| meta.get(key).filter(|v| !v.is_null());
        for key in OFF_DEVICE_FIELDS {
            if set(key).is_some() {
                return Some(format!(
                    "{NAME}: `{key}` has no meaning here — the model is Apple's on-device \
                     system model (no endpoint, no model choice, no credentials)"
                ));
            }
        }
        if let Some(v) = set("temperature") {
            if !number(v).is_some_and(|t| t.is_finite() && t >= 0.0) {
                return Some("temperature must be a number >= 0".to_string());
            }
        }
        if let Some(v) = set("max_tokens") {
            if !int(v).is_some_and(|n| n >= 1) {
                return Some("max_tokens must be a positive int".to_string());
            }
        }
        if let Some(v) = set("context_window") {
            let size = self.engine.context_size();
            if !int(v).is_some_and(|n| n >= 1 && n <= size) {
                return Some(format!(
                    "context_window must be a positive int no larger than the on-device \
                     model's window ({size} tokens)"
                ));
            }
        }
        schema::validate(
            &Self::schema_for_window(self.engine.context_size()),
            NAME,
            meta,
            record,
        )
    }

    fn record_schema(&self) -> Option<Value> {
        Some(Self::schema_for_window(self.engine.context_size()))
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if verb == "reflect" {
            return Ok(Some(self.reflect_reply(agent_id, kernel)));
        }
        // EVERY OTHER ai_core VERB, one table for every backend.
        let provider = |_client: &str| self.build_provider(agent_id, kernel);
        if let Some(reply) =
            verbs::handle_common(agent_id, payload, kernel, self.cfg(), provider).await?
        {
            return Ok(Some(reply));
        }
        Ok(Some(json!({
            "error": format!(
                "{NAME}: unknown type {}",
                py_repr_value(&payload.get("type").cloned().unwrap_or(Value::Null))
            )
        })))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        state::drop_state(agent_id);
        Ok(())
    }

    /// The kernel is killing a task of this agent: close the turn out for the
    /// caller. The kernel then DROPS the generation's future, and with it the
    /// [`FmStream`], whose drop cancels the on-device session (`fm_cancel`).
    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await;
        Ok(())
    }
}

// ── the provider ────────────────────────────────────────────────────

/// ai-core's chat messages -> one FMCore request, streamed as RAW TEXT.
///
/// THE SPLIT (swift's `FoundationModelsProvider.request`, exactly):
///   * every `system` message -> `instructions`, joined with a blank line, in order;
///   * the LAST non-system message must be `user` -> `prompt`;
///   * every other non-system message, in order -> `turns` (`user`/`assistant`).
pub struct FoundationModelsProvider {
    engine: Arc<dyn FmEngine>,
    temperature: Option<f64>,
    max_tokens: Option<i64>,
    /// Set when the model was unavailable as the provider was built.
    refusal: Option<ProviderError>,
    /// The model's own count of the last prompt (macOS 27+), for ai-core's
    /// calibration. FM reports no stop reason, so none is claimed.
    report: ReportCell,
}

impl FoundationModelsProvider {
    /// The request for `messages`, or the refusal for a list the session cannot
    /// answer.
    ///
    /// # Errors
    /// A list whose last non-system message is not a `user` turn.
    pub fn request(
        messages: &[Value],
        temperature: Option<f64>,
        max_tokens: Option<i64>,
    ) -> Result<FmRequest, ProviderError> {
        let role = |m: &Value| m.get("role").cloned().unwrap_or(Value::Null);
        let content = |m: &Value| m.get("content").map_or_else(String::new, py_str);
        let is_system = |m: &Value| m.get("role").and_then(Value::as_str) == Some("system");
        let instructions: Vec<String> = messages
            .iter()
            .filter(|m| is_system(m))
            .map(content)
            .collect();
        let rest: Vec<&Value> = messages.iter().filter(|m| !is_system(m)).collect();
        let Some((last, earlier)) = rest
            .split_last()
            .filter(|(last, _)| last.get("role").and_then(Value::as_str) == Some("user"))
        else {
            let r = rest
                .last()
                .map_or_else(|| "none".to_string(), |m| py_str(&role(m)));
            return Err(ProviderError::raised(format!(
                "{NAME}: the last message is not a user turn (role {r}) — the on-device \
                 session answers a prompt"
            )));
        };
        Ok(FmRequest {
            instructions: instructions.join("\n\n"),
            turns: earlier
                .iter()
                .map(|m| FmTurn {
                    role: py_str(&role(m)),
                    content: content(m),
                })
                .collect(),
            prompt: content(last),
            temperature,
            max_tokens,
        })
    }
}

#[async_trait]
impl Provider for FoundationModelsProvider {
    fn model(&self) -> String {
        MODEL.to_string()
    }

    fn ready(&self) -> Result<(), ProviderError> {
        self.refusal.clone().map_or(Ok(()), Err)
    }

    fn report(&self) -> ProviderReport {
        self.report.lock().expect("report poisoned").clone()
    }

    fn clear_prompt_tokens(&self) {
        self.report.lock().expect("report poisoned").prompt_tokens = None;
    }

    /// `max_tokens` bounds a side call (a fold summary); `None`, the record's
    /// `max_tokens` (or the model's own) bounds the turn.
    async fn chat(
        &self,
        messages: &[Value],
        max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        self.ready()?;
        let request = Self::request(messages, self.temperature, max_tokens.or(self.max_tokens))?;
        Ok(events(
            self.engine.stream(&request),
            Arc::clone(&self.report),
        ))
    }
}

/// The engine's events as ai-core's token stream. A failure is raised under this
/// bundle's name with the bridge's words — what swift raises from FMCore. The
/// model's prompt count is kept only from a generation that finished.
fn events(stream: FmStream, report: ReportCell) -> ProviderStream {
    Box::pin(futures_util::stream::unfold(
        Some((stream, report)),
        |state| async move {
            let (mut stream, report) = state?;
            let failed =
                |e: String| Some((Err(ProviderError::raised(format!("{NAME}: {e}"))), None));
            match stream.next().await {
                Some(FmEvent::Text(t)) => {
                    Some((Ok(ProviderEvent::Token(t)), Some((stream, report))))
                }
                Some(FmEvent::Done {
                    error: None,
                    prompt_tokens,
                }) => {
                    if prompt_tokens >= 0 {
                        report.lock().expect("report poisoned").prompt_tokens = Some(prompt_tokens);
                    }
                    None
                }
                Some(FmEvent::Done { error: Some(e), .. } | FmEvent::Broken(e)) => failed(e),
                None => failed("the on-device stream ended without a result".to_string()),
            }
        },
    ))
}

#[cfg(test)]
mod tests;
#[cfg(all(test, target_os = "macos"))]
mod tests_bridge;
#[cfg(test)]
mod tests_fake;
