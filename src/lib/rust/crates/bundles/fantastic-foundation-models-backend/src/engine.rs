//! The ENGINE SEAM — what the bundle calls to reach the on-device model.
//!
//! [`crate::system::SystemEngine`] is the real one: the swift `FMCore` through the
//! `FantasticFMBridge` C ABI on macOS, a refusal naming the platform everywhere
//! else. The unit suite injects a fake through [`FmEngine`], so the default suite
//! never needs the model, Apple Intelligence or macOS 26.
//!
//! The words here are FMCore's (`src/lib/swift/Sources/FantasticFMCore/FMCore.swift`),
//! spelled once: the bridge sends them across the C ABI, and the few this crate
//! must produce itself (a platform the bridge is not built for) use the same
//! sentences.

use serde_json::{json, Value};
use tokio::sync::mpsc;

/// The on-device model's window as Apple DOCUMENTS it (TN3193), in tokens —
/// only where no SDK can be asked, where every send is refused anyway. Swift
/// `FM_DOCUMENTED_CONTEXT_SIZE`.
pub const FM_DOCUMENTED_CONTEXT_SIZE: i64 = 4096;

/// The message a stream that was cancelled ends with. Swift `FM_CANCELLED`.
pub const FM_CANCELLED: &str = "cancelled";

/// Whether the on-device model can answer right now — or the exact reason it
/// cannot, as one sentence that names it. Swift `FMAvailability`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FmAvailability {
    /// The model can answer.
    Available,
    /// It cannot; the sentence names why ([`FmReason`]).
    Unavailable(String),
}

impl FmAvailability {
    /// Whether the model can answer.
    #[must_use]
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// The reason, or `None` when available.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Unavailable(r) => Some(r),
        }
    }

    /// Read `fm_availability`'s `{"available":bool,"reason":str|null}`. A
    /// document of any other shape is a broken bridge, and is said so — never
    /// read as available.
    #[must_use]
    pub fn from_bridge_json(raw: &str) -> Self {
        let parsed: Option<Value> = serde_json::from_str(raw).ok();
        let available = parsed
            .as_ref()
            .and_then(|v| v.get("available"))
            .and_then(Value::as_bool);
        let reason = parsed
            .as_ref()
            .and_then(|v| v.get("reason"))
            .and_then(Value::as_str);
        match (available, reason) {
            (Some(true), _) => Self::Available,
            (Some(false), Some(r)) => Self::Unavailable(r.to_string()),
            _ => Self::Unavailable(format!(
                "fm_availability answered something that is not {{available, reason}}: {raw}"
            )),
        }
    }
}

/// The reason sentences — FMCore's `FMReason`, ONE spelling each.
pub struct FmReason;

impl FmReason {
    /// The device cannot run Apple Intelligence.
    pub const DEVICE_NOT_ELIGIBLE: &'static str =
        "device not eligible — this device cannot run Apple Intelligence";
    /// Apple Intelligence is off.
    pub const APPLE_INTELLIGENCE_NOT_ENABLED: &'static str =
        "Apple Intelligence is not enabled — turn it on in System Settings > Apple Intelligence & Siri";
    /// The model is still downloading or preparing.
    pub const MODEL_NOT_READY: &'static str = "model not ready — the on-device model is still downloading or preparing; retry once it is ready";

    /// Not macOS 26 or later, naming what it is instead.
    #[must_use]
    pub fn platform(what: &str) -> String {
        format!("platform not macOS 26+ — this is {what}")
    }
}

/// The refusal for an unavailable model — the reason, prefixed once. Swift
/// `fmUnavailableMessage`.
#[must_use]
pub fn unavailable_message(reason: &str) -> String {
    format!("the on-device model is unavailable: {reason}")
}

/// One prior turn: `user` (a prompt the model was given) or `assistant` (what it
/// answered). System text is never a turn — it is the request's `instructions`.
#[derive(Clone, Debug, PartialEq)]
pub struct FmTurn {
    /// `user` or `assistant`.
    pub role: String,
    /// The turn's text.
    pub content: String,
}

/// One generation: the instructions, the prior turns (replayed into the
/// session's transcript, in order), the prompt to answer, and the two options.
/// `None` = the model's own. Swift `FMRequest`.
#[derive(Clone, Debug, PartialEq)]
pub struct FmRequest {
    /// Every system message, joined by a blank line.
    pub instructions: String,
    /// The conversation before the prompt.
    pub turns: Vec<FmTurn>,
    /// The text answered.
    pub prompt: String,
    /// Sampling temperature (>= 0).
    pub temperature: Option<f64>,
    /// The output budget, in tokens (>= 1).
    pub max_tokens: Option<i64>,
}

impl FmRequest {
    /// The bridge's `request_json`, keys in the header's order:
    /// `{"instructions","turns":[{"role","content"}],"prompt","temperature","max_tokens"}`
    /// — an unset option is `null` (the model's own).
    #[must_use]
    pub fn to_json(&self) -> String {
        let turns: Vec<Value> = self
            .turns
            .iter()
            .map(|t| json!({"role": t.role, "content": t.content}))
            .collect();
        json!({
            "instructions": self.instructions,
            "turns": turns,
            "prompt": self.prompt,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
        })
        .to_string()
    }
}

/// One event off a generation, in order: text DELTAS, then `Done` exactly once,
/// last — the bridge's `on_chunk` / `on_done`.
#[derive(Clone, Debug, PartialEq)]
pub enum FmEvent {
    /// New text since the last delta. Never empty.
    Text(String),
    /// The bridge broke its contract mid-generation (a delta that is not
    /// UTF-8). Terminal for the reader; the generation itself is still running
    /// and is cancelled when the stream is dropped.
    Broken(String),
    /// The end: `error` `None` on success (else the bridge's sentence,
    /// [`FM_CANCELLED`] after a cancel); `prompt_tokens` the model's own count
    /// of the whole prompt, or -1 when it reported none.
    Done {
        /// Why it failed, or `None`.
        error: Option<String>,
        /// The model's prompt count, or -1.
        prompt_tokens: i64,
    },
}

/// A live generation. Events arrive on a channel the engine's callbacks feed;
/// DROPPING the stream before its `Done` cancels the generation (`fm_cancel`) —
/// which is how a kernel `cancel` of the `send` task, a timeout, or a caller that
/// stops reading reaches the on-device session.
pub struct FmStream {
    rx: mpsc::UnboundedReceiver<FmEvent>,
    cancel: Option<Box<dyn FnOnce() + Send>>,
}

impl FmStream {
    /// A stream over `rx`; `cancel` runs once if the stream is dropped before
    /// its `Done` arrived.
    pub fn new(
        rx: mpsc::UnboundedReceiver<FmEvent>,
        cancel: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            rx,
            cancel: Some(Box::new(cancel)),
        }
    }

    /// A stream that has already ended with `error` — nothing was started, so
    /// there is nothing to cancel.
    #[must_use]
    pub fn failed(error: String) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        // The receiver is alive here; a send cannot fail.
        let _ = tx.send(FmEvent::Done {
            error: Some(error),
            prompt_tokens: -1,
        });
        Self { rx, cancel: None }
    }

    /// Cancel the generation now and keep reading: its `Done` follows, with
    /// [`FM_CANCELLED`] (the bridge's `fm_cancel`). A second call, or one after
    /// `Done`, does nothing.
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }

    /// The next event; `None` once the engine is gone. After `Done` (or `None`)
    /// the generation is over and a drop cancels nothing.
    pub async fn next(&mut self) -> Option<FmEvent> {
        let ev = self.rx.recv().await;
        if matches!(ev, Some(FmEvent::Done { .. }) | None) {
            self.cancel = None;
        }
        ev
    }
}

impl Drop for FmStream {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

/// What the bundle calls. Swift `FMEngine`.
pub trait FmEngine: Send + Sync + 'static {
    /// Available, or the exact reason it is not.
    fn availability(&self) -> FmAvailability;

    /// The model's context window in tokens — read from the SDK where one
    /// exists ([`FM_DOCUMENTED_CONTEXT_SIZE`] where none can be asked).
    fn context_size(&self) -> i64;

    /// Start one generation, streamed as RAW TEXT DELTAS. Every failure —
    /// an unavailable model, an invalid request, the model's own — arrives as
    /// the stream's `Done { error }`.
    fn stream(&self, request: &FmRequest) -> FmStream;
}
