//! The generation: ONE provider pass per `send` — assemble the prompt,
//! auto-compact to fit the window, stream tokens, persist history (RAW text out
//! to the caller, `<think>`-stripped text into the store). Per-backend knobs ride
//! on [`BackendConfig`].
//!
//! ONE PASS, and still no loop HERE — but that is not the same as "nothing
//! happens". This module generates; [`crate::route::route_reply`] then dispatches
//! whatever the completion named and delivers each reply to its own `reply_to`. A
//! chain therefore advances one pass at a time, and it advances only because the
//! model asked again: no call ends the turn, and stop is the default. Nothing
//! iterates on the model's behalf.

use crate::assembly::assemble_messages;
use crate::context::estimate_tokens;
use crate::events::{emit_status, to_caller};
use crate::memory::{save_history, MemoryStoreError};
use crate::provider::{Provider, ProviderError, ProviderEvent};
use crate::state::BackendState;
use crate::think::without_thinking;
use fantastic_kernel::{AgentId, Kernel};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Per-backend knobs threaded through the shared `send` so a single path
/// serves every backend. The provider is the streaming seam.
#[derive(Clone, Copy, Debug)]
pub struct BackendConfig {
    /// Backend error-message prefix (e.g. `"ollama_backend"`), used by the
    /// context-projection seam's `too_small` / config-error messages.
    pub name: &'static str,
    /// python `Backend.provider_missing_error`, set only by a backend whose
    /// `require_provider` is true (nvidia, whose key is the precondition).
    /// `None` = this backend can always build a provider, so `send` has nothing
    /// to failfast on and never reads this.
    pub provider_missing_error: Option<&'static str>,
}

/// Why a generation produced no completion. The two kinds settle differently,
/// so they are not one string: python returns a projection refusal VERBATIM out
/// of `_run` (no status event, no `client_id`, no raise), while a provider
/// failure either maps to a caller-facing sentence or propagates.
pub enum GenFailure {
    /// The context-projection seam refused (`too_small` failsafe, unknown
    /// strategy). The dict is the reply — python `_run`'s
    /// `if isinstance(projected, dict): return projected`.
    Projection(Value),
    /// The provider pass failed. See [`ProviderError`].
    Provider(ProviderError),
    /// The agent's memory store refused or is unreachable — FAIL FAST, no turn
    /// (python `MemoryStoreError`, caught by `_send`).
    Store(MemoryStoreError),
}

/// What one generation produced.
pub enum Generation {
    /// An ANSWER, stored as this turn's assistant row `message_id`. `text` is
    /// RAW — thinking markers intact — because routing parses it.
    Answered {
        /// The raw completion text.
        text: String,
        /// The assistant row this turn was stored as.
        message_id: i64,
    },
    /// A DEAD turn: nothing left once the thinking is stripped. DISCARDED —
    /// never stored, never an answer — and routed like any turn that dispatched
    /// nothing, so the no-call reminder rerolls it (python `_run`'s
    /// `dead_turn` reply).
    Dead {
        /// The raw completion text (thinking only, or nothing).
        text: String,
    },
}

impl From<MemoryStoreError> for GenFailure {
    fn from(e: MemoryStoreError) -> Self {
        Self::Store(e)
    }
}

/// Project the message list to what the model actually reads — its role and its
/// text; every other key is stripped.
fn render_for_model(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let role = m.get("role").cloned().unwrap_or(Value::Null);
            let content = m.get("content").cloned().unwrap_or_else(|| json!(""));
            json!({"role": role, "content": content})
        })
        .collect()
}

/// One generation: assemble (system + compacted history + user) → project context
/// ONCE (auto-compact / notice / too_small) → ONE provider pass streaming tokens →
/// persist. Runs INLINE in the `send` dispatch — which is itself a kernel task, so
/// `cancel` drops this future (and with it the provider stream).
///
/// Returns the RAW completion text, thinking markers intact: [`crate::verbs::send`]
/// hands it to the router, and a tool call the model wrote mid-thought must still
/// dispatch. Only the DURABLE turn is stripped.
///
/// The conversation is ALWAYS this agent's own (its memory store): there is no
/// caller-supplied history — a one-off completion is a disposable agent with its
/// own store; context for another agent travels in the text, or by grant
/// (`share` → `fork_from`).
#[allow(clippy::too_many_arguments)]
pub async fn run_generation(
    provider: &Arc<dyn Provider>,
    self_id: &AgentId,
    state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
    cfg: BackendConfig,
    system_prompt: Option<String>,
) -> Result<Generation, GenFailure> {
    // A RECALL OWED FROM THE LAST FOLD BATCH (or from a boot) runs first, so this
    // turn is assembled over the head the recall chose — python `_recall_due`.
    let due = std::mem::take(&mut *state.recall_due.lock().expect("recall_due poisoned"));
    if due {
        crate::retrieval::context_recall(self_id, &json!({}), kernel, Some(provider)).await?;
    }
    let (messages, store_len) = assemble_messages(
        self_id,
        state,
        user_text,
        kernel,
        client_id,
        system_prompt.as_deref(),
    )
    .await?;
    // Context-overflow projection: shape what the MODEL sees this turn to fit the
    // window via the agent's `context_strategy`. Done ONCE at turn entry. The
    // window is read off the record EVERY turn, so a model switched to a smaller
    // one (`update_agent`) is fitted — folded — on its very next turn.
    let model_messages = match crate::projection::project_context(
        provider, self_id, state, kernel, client_id, cfg.name, messages, store_len,
    )
    .await
    {
        Ok(m) => m,
        // too_small failsafe / unknown-strategy config error — the model is NOT
        // called and the seam's dict IS the reply, python-verbatim: no
        // `status(done)` closes a turn that never opened a provider pass, and no
        // `client_id` is stapled onto a refusal python returns bare.
        Err(crate::projection::ProjectFail::Refusal(e)) => return Err(GenFailure::Projection(e)),
        Err(crate::projection::ProjectFail::Store(e)) => return Err(GenFailure::Store(e)),
    };

    // ONE provider pass. Every chunk is a content token — the provider knows
    // nothing of the tool protocol; the envelope is text this crate parses after.
    let model_view = render_for_model(&model_messages);
    // A FRESH reading per pass: a provider that reports nothing this time must not
    // leave last pass's number standing.
    provider.clear_prompt_tokens();
    let mut stream = provider
        .chat(&model_view, None)
        .await
        .map_err(GenFailure::Provider)?;
    let mut content_parts: Vec<String> = Vec::new();
    let mut first_text_chunk = true;
    while let Some(ev) = stream.next().await {
        match ev.map_err(GenFailure::Provider)? {
            ProviderEvent::Token(t) => {
                if first_text_chunk {
                    first_text_chunk = false;
                    emit_status(kernel, state, self_id, client_id, "streaming", Map::new()).await;
                }
                {
                    let mut cur = state.current_meta.lock().expect("current poisoned");
                    if let Some(c) = cur.as_mut() {
                        c.text_so_far.push_str(&t);
                    }
                }
                to_caller(
                    kernel,
                    self_id,
                    client_id,
                    json!({"type": "token", "text": t, "source": self_id.as_str()}),
                )
                .await;
                content_parts.push(t);
            }
        }
    }
    let last_text = content_parts.join("");
    // THE ENDPOINT'S OWN COUNT calibrates our chars/4 estimate (python `_calib`).
    if let Some(reported) = provider.report().prompt_tokens.filter(|n| *n > 0) {
        let est = estimate_tokens(&model_view).max(1);
        *state.calib.lock().expect("calib poisoned") = Some(reported as f64 / est as f64);
        *state.reported.lock().expect("reported poisoned") = Some(reported);
    }

    // A DEAD TURN — nothing left once the thinking is stripped — did not answer,
    // whatever the provider's finish reason says. Measured 2026-09-30 on vLLM: a
    // thinking budget of 8 tokens ended the thought, the model stopped cleanly
    // (`finish_reason: stop`) with an empty answer, and the turn was stored as a
    // completed, empty assistant row. The ANSWER is discarded here — never stored,
    // never counted as an answer — and the turn is routed like any that
    // dispatched nothing: the no-call reminder rerolls it (owner, 2026-09-30;
    // python `_run`). Failing the turn outright ended a 30-pass chain on its first
    // dead turn (E31, measured).
    if without_thinking(&last_text).trim().is_empty() {
        // The INCOMING row is kept — only the empty answer is dropped. The turn
        // this one answered may be a reply batch carrying the results of the
        // model's own calls; unstored, the reroll would arrive with those results
        // gone and the model would re-ask for them (found in the swift port's
        // review, 2026-09-30).
        let in_id = kernel.mint_id();
        let in_parent = incoming_parent(user_text);
        save_history(
            self_id,
            kernel,
            &[json!({"role": "user", "content": user_text, "id": in_id, "parent": in_parent})],
        )
        .await?;
        emit_status(kernel, state, self_id, client_id, "done", {
            let mut m = Map::new();
            m.insert("reason".to_string(), json!("dead_turn"));
            m
        })
        .await;
        return Ok(Generation::Dead { text: last_text });
    }

    emit_status(kernel, state, self_id, client_id, "done", {
        let mut m = Map::new();
        m.insert("reason".to_string(), json!("ok"));
        m
    })
    .await;

    // ROW IDS for this turn: the incoming row (a root, or a delivered reply turn
    // that names its parent on its tag) and the assistant row that answers it.
    //
    // MINTED HERE, AT THE APPEND — not at turn start. The store returns an
    // owner's rows in ID order and the head cache keeps APPEND order; the two
    // agree only while ids are minted as rows are appended. Minted at turn start,
    // a fold or view row saved mid-turn (the projection) took a higher id yet
    // landed first, and after a restart the conversation came back in a
    // different order than the agent had lived it (python, E78).
    let in_id = kernel.mint_id();
    let in_parent = incoming_parent(user_text);
    let message_id = kernel.mint_id();
    // APPEND this send's user + assistant turns to the memory store so context
    // management (compaction/recall) has the FULL conversation next turn.
    let new_turns = vec![
        json!({"role": "user", "content": user_text, "id": in_id, "parent": in_parent}),
        // STRIPPED — the deliberation is not part of the conversation. The RAW
        // text (thinking included) is what gets parsed and what the caller
        // receives; only durable history loses the `<think>` spans, because a
        // reasoning model re-fed its own prior deliberation drowns in it
        // (gemma4:12b re-sent a 3,800-char turn-1 thought every later prompt,
        // 2026-08-02).
        json!({"role": "assistant", "content": without_thinking(&last_text), "id": message_id, "parent": in_id}),
    ];
    save_history(self_id, kernel, &new_turns).await?;
    Ok(Generation::Answered {
        text: last_text,
        message_id,
    })
}

/// The turn a delivered reply answers: `↑N` on the FIRST tag of the incoming
/// text — python `_TAG_LINKS`. Anything else is a ROOT.
fn incoming_parent(text: &str) -> Option<i64> {
    let rest = text.strip_prefix("[reply to ")?;
    let head = &rest[..rest.find(']')?];
    let (_, after) = head.split_once('↑')?;
    let tok: String = after.chars().take_while(|c| !c.is_whitespace()).collect();
    tok.parse::<i64>().ok()
}
