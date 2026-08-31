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
use crate::events::{emit_status, to_caller, CallerRoute};
use crate::history::append_history;
use crate::provider::{Provider, ProviderEvent};
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
    /// How streaming events reach the caller.
    pub route: CallerRoute,
    /// Backend error-message prefix (e.g. `"ollama_backend"`), used by the
    /// context-projection seam's `too_small` / config-error messages.
    pub name: &'static str,
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
) -> Result<String, String> {
    // `messages` is the FULL conversation (persistence source). Projection shapes
    // the MODEL view to fit the window ONCE at entry, prepending the canonical
    // [context-notice]; the durable store is never trimmed by it. A caller-supplied
    // `system_prompt` replaces the default system block (a caller teaches its tools).
    let messages = assemble_messages(
        self_id,
        state,
        user_text,
        kernel,
        client_id,
        system_prompt.as_deref(),
    )
    .await;
    let model_messages = match crate::projection::project_context(
        provider, self_id, state, kernel, client_id, cfg.route, cfg.name, messages,
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            // too_small failsafe / unknown-strategy config error — the model is NOT
            // called. Surface it as the send error (the seam already pushed the event).
            return Err(e
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("context projection error")
                .to_string());
        }
    };

    // ONE provider pass. Every chunk is a content token — the provider knows
    // nothing of the tool protocol; the envelope is text this crate parses after.
    let model_view = render_for_model(&model_messages);
    let mut stream = provider.chat(&model_view).await?;
    let mut content_parts: Vec<String> = Vec::new();
    let mut first_text_chunk = true;
    while let Some(ev) = stream.next().await {
        match ev? {
            ProviderEvent::Token(t) => {
                if first_text_chunk {
                    first_text_chunk = false;
                    emit_status(
                        kernel,
                        state,
                        self_id,
                        client_id,
                        cfg.route,
                        "streaming",
                        Map::new(),
                    )
                    .await;
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
                    cfg.route,
                    json!({"type": "token", "text": t, "source": self_id.as_str()}),
                )
                .await;
                content_parts.push(t);
            }
        }
    }
    let last_text = content_parts.join("");

    emit_status(kernel, state, self_id, client_id, cfg.route, "done", {
        let mut m = Map::new();
        m.insert("reason".to_string(), json!("ok"));
        m
    })
    .await;

    // APPEND this send's user + assistant turns to the durable chat store so
    // context management (compaction/recall) has the FULL conversation next turn.
    let new_turns = vec![
        json!({"role": "user", "content": user_text}),
        // STRIPPED — the deliberation is not part of the conversation. The RAW
        // text (thinking included) is what gets parsed and what the caller
        // receives; only durable history loses the `<think>` spans, because a
        // reasoning model re-fed its own prior deliberation drowns in it
        // (gemma4:12b re-sent a 3,800-char turn-1 thought every later prompt,
        // 2026-08-02).
        json!({"role": "assistant", "content": without_thinking(&last_text)}),
    ];
    append_history(self_id, kernel, client_id, &new_turns).await;
    Ok(last_text)
}
