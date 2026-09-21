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
use crate::events::{emit_status, to_caller};
use crate::history::append_history;
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
}

/// Project the message list to what the model actually reads — pure text, every
/// role a chat template renders. Maps any `role:tool` (legacy turns in an old
/// chat.json) to `role:user`, because many templates don't render a `tool` role.
fn render_for_model(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("");
            let out_role = if role == "tool" { "user" } else { role };
            let content = m.get("content").cloned().unwrap_or_else(|| json!(""));
            json!({"role": out_role, "content": content})
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
    messages_override: Option<Vec<Value>>,
) -> Result<(String, i64), GenFailure> {
    // STATELESS: a caller-supplied non-empty `messages` list means the caller owns
    // this turn's context. No owed recall runs, no projection shapes it, and
    // NOTHING is persisted — python `_run`'s `stateless`.
    let stateless = messages_override.as_ref().is_some_and(|m| !m.is_empty());
    // `messages` is the FULL conversation (persistence source). Projection shapes
    // the MODEL view to fit the window ONCE at entry, prepending the canonical
    // [context-notice]; the durable store is never trimmed by it. A caller-supplied
    // `system_prompt` replaces the default system block (a caller teaches its tools).
    // A RECALL OWED FROM THE LAST FOLD BATCH runs first, so this turn is
    // assembled over the head the recall chose — python `_recall_due`.
    let due = std::mem::take(&mut *state.recall_due.lock().expect("recall_due poisoned"));
    if due && !stateless {
        let _ = crate::retrieval::context_recall(self_id, &json!({}), kernel, Some(provider)).await;
    }
    let (messages, store_len) = assemble_messages(
        self_id,
        state,
        user_text,
        kernel,
        client_id,
        system_prompt.as_deref(),
        messages_override.as_deref(),
    )
    .await;
    // ROW IDS FOR THIS TURN — python `_row_ids_for_incoming` + `_mint_row_id`:
    // the incoming row (a root, or a delivered reply that names the turn it
    // answers with `↑N` on its first tag) and the assistant row that answers
    // it. Pre-minted so this turn's reply tags can name the assistant row
    // before it exists, and so every row carries the `parent` the fold
    // machinery derives chains from.
    //
    // MINTED AFTER ASSEMBLY, as python does: the system block is rebuilt from
    // the live tree every turn and its two `reflect` dispatches take their call
    // ids FIRST. Minting here first put this turn's user + assistant rows two
    // ids ahead of python's on every single turn (measured in the r3 capture:
    // `message_id` 740 where python had 742).
    let in_id = kernel.rows.mint_id();
    let in_parent = incoming_parent(user_text);
    let message_id = kernel.rows.mint_id();
    let model_messages = if stateless {
        messages
    } else {
        match crate::projection::project_context(
            provider, self_id, state, kernel, client_id, cfg.name, messages, store_len,
        )
        .await
        {
            Ok(m) => m,
            // too_small failsafe / unknown-strategy config error — the model is
            // NOT called and the seam's dict IS the reply, python-verbatim: no
            // `status(done)` closes a turn that never opened a provider pass,
            // and no `client_id` is stapled onto a refusal python returns bare.
            Err(e) => return Err(GenFailure::Projection(e)),
        }
    };

    // ONE provider pass. Every chunk is a content token — the provider knows
    // nothing of the tool protocol; the envelope is text this crate parses after.
    let model_view = render_for_model(&model_messages);
    let mut stream = provider
        .chat(&model_view)
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

    emit_status(kernel, state, self_id, client_id, "done", {
        let mut m = Map::new();
        m.insert("reason".to_string(), json!("ok"));
        m
    })
    .await;

    // APPEND this send's user + assistant turns to the durable chat store so
    // context management (compaction/recall) has the FULL conversation next turn.
    if stateless {
        return Ok((last_text, message_id));
    }
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
    append_history(self_id, kernel, client_id, &new_turns).await;
    Ok((last_text, message_id))
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
