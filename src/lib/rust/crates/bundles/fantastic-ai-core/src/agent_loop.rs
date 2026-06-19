//! The agentic loop: drive `provider.chat()`, stream tokens, dispatch
//! tool-calls, persist history, terminate when the model stops emitting
//! tools. Shared by every backend; per-backend knobs ride on
//! [`BackendConfig`].

use crate::assembly::{assemble_messages, send_tool_def};
use crate::events::{emit_done, emit_status, to_caller, CallerRoute};
use crate::history::save_history;
use crate::provider::{Provider, ProviderEvent};
use crate::state::BackendState;
use fantastic_kernel::{AgentId, Kernel};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Per-backend knobs threaded through the shared loop + verbs so a
/// single `send` serves every backend. The provider is the streaming
/// seam; this carries the few behavioural differences that remain.
#[derive(Clone, Copy, Debug)]
pub struct BackendConfig {
    /// How streaming events reach the caller.
    pub route: CallerRoute,
    /// Serialize tool_call arguments to a JSON string in the persisted
    /// assistant turn (OpenAI shape — NIM). When `false`, arguments are
    /// embedded as a JSON object (ollama / Python reference shape).
    pub tool_args_as_json: bool,
    /// Dispatch a batch of tool_calls in parallel (ollama) vs serially
    /// (NIM). Both preserve model-emitted order in the appended
    /// `role:tool` messages.
    pub parallel_tools: bool,
    /// Backend error-message prefix (e.g. `"ollama_backend"`), used by the
    /// context-projection seam's `too_small` / config-error messages.
    pub name: &'static str,
}

/// Shared references bound for the duration of one generation. Cuts the
/// argument count on the inner loop helpers + keeps the dispatch sites
/// readable.
struct LoopCtx<'a> {
    provider: &'a Arc<dyn Provider>,
    self_id: &'a AgentId,
    state: &'a Arc<BackendState>,
    kernel: &'a Arc<Kernel>,
    client_id: &'a str,
    cfg: BackendConfig,
}

/// One resolved tool-call from a provider pass.
struct ToolCall {
    id: String,
    name: String,
    args: Value,
}

/// Drive one provider pass: stream tokens to the caller, accumulate
/// content + finalized tool-calls. Returns `(content, tool_calls)`.
async fn run_pass(
    ctx: &LoopCtx<'_>,
    messages: &[Value],
    tools: &[Value],
) -> Result<(String, Vec<ToolCall>), String> {
    let mut stream = ctx.provider.chat(messages, tools).await?;
    let mut content_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut first_text_chunk = true;
    while let Some(ev) = stream.next().await {
        match ev? {
            ProviderEvent::Token(t) => {
                if first_text_chunk {
                    first_text_chunk = false;
                    emit_status(
                        ctx.kernel,
                        ctx.state,
                        ctx.self_id,
                        ctx.client_id,
                        ctx.cfg.route,
                        "streaming",
                        Map::new(),
                    )
                    .await;
                }
                {
                    let mut cur = ctx.state.current_meta.lock().expect("current poisoned");
                    if let Some(c) = cur.as_mut() {
                        c.text_so_far.push_str(&t);
                    }
                }
                to_caller(
                    ctx.kernel,
                    ctx.self_id,
                    ctx.client_id,
                    ctx.cfg.route,
                    json!({"type": "token", "text": t, "source": ctx.self_id.as_str()}),
                )
                .await;
                content_parts.push(t);
            }
            ProviderEvent::ToolCall { id, name, args } => {
                tool_calls.push(ToolCall { id, name, args });
            }
        }
    }
    Ok((content_parts.join(""), tool_calls))
}

/// Encode one tool-call into the assistant-turn `tool_calls` shape.
fn assistant_tool_call(c: &ToolCall, tool_args_as_json: bool) -> Value {
    let arguments = if tool_args_as_json {
        json!(serde_json::to_string(&c.args).unwrap_or_else(|_| "{}".to_string()))
    } else {
        c.args.clone()
    };
    json!({
        "id": c.id,
        "type": "function",
        "function": {"name": c.name, "arguments": arguments},
    })
}

/// Dispatch one tool-call: emit entry/exit status, run the kernel.send,
/// emit the `say` summary, and return the appended `role:tool` message.
async fn dispatch_one(ctx: &LoopCtx<'_>, c: &ToolCall) -> Value {
    let target = c
        .args
        .get("target_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let payload = c.args.get("payload").cloned().unwrap_or(Value::Null);
    let verb = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tool_entry = json!({
        "call_id": c.id,
        "target": target,
        "verb": verb,
        "args": c.args,
    });
    {
        let mut cur = ctx.state.current_meta.lock().expect("current poisoned");
        if let Some(e) = cur.as_mut() {
            e.last_tool = Some(tool_entry.clone());
        }
    }
    let mut entry_detail = Map::new();
    entry_detail.insert("tool".to_string(), tool_entry.clone());
    emit_status(
        ctx.kernel,
        ctx.state,
        ctx.self_id,
        ctx.client_id,
        ctx.cfg.route,
        "tool_calling",
        entry_detail,
    )
    .await;

    let reply = if target.is_empty() {
        json!({"error": "empty target_id"})
    } else {
        ctx.kernel
            .send(&AgentId::from(target.as_str()), payload)
            .await
    };
    let reply_str = serde_json::to_string(&reply).unwrap_or_else(|_| "{}".to_string());
    let preview: String = reply_str.chars().take(120).collect();

    let mut tool_done = tool_entry.clone();
    if let Some(o) = tool_done.as_object_mut() {
        o.insert("reply_preview".to_string(), json!(preview.clone()));
    }
    {
        let mut cur = ctx.state.current_meta.lock().expect("current poisoned");
        if let Some(e) = cur.as_mut() {
            e.last_tool = Some(tool_done.clone());
        }
    }
    let mut exit_detail = Map::new();
    exit_detail.insert("tool".to_string(), tool_done);
    emit_status(
        ctx.kernel,
        ctx.state,
        ctx.self_id,
        ctx.client_id,
        ctx.cfg.route,
        "tool_calling",
        exit_detail,
    )
    .await;
    to_caller(
        ctx.kernel,
        ctx.self_id,
        ctx.client_id,
        ctx.cfg.route,
        json!({
            "type": "say",
            "text": format!("[tool {} -> {}]", target, preview),
            "source": ctx.self_id.as_str(),
        }),
    )
    .await;

    json!({
        "role": "tool",
        "tool_call_id": c.id,
        "name": c.name,
        "content": reply_str,
    })
}

/// The core streaming + tool-call loop. Runs inside the spawned task so
/// `interrupt` can abort cleanly. Returns the final assistant text.
#[allow(clippy::too_many_arguments)]
pub async fn run_generation(
    provider: &Arc<dyn Provider>,
    self_id: &AgentId,
    state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
    cfg: BackendConfig,
) -> Result<String, String> {
    let ctx = LoopCtx {
        provider,
        self_id,
        state,
        kernel,
        client_id,
        cfg,
    };
    // `messages` is the FULL conversation (persistence source). `model_messages`
    // is the projected view the model actually sees — the Context-Protocol seam
    // shapes it to fit the window ONCE at entry (never mid-tool-loop, which would
    // orphan a role:tool), prepending the canonical [context-notice]. New turns
    // this send produces are appended to BOTH so the durable store stays whole
    // while the model context stays bounded. The notice lives ONLY in the model
    // view — it is never persisted.
    let mut messages = assemble_messages(self_id, state, user_text, kernel, client_id).await;
    let mut model_messages = match crate::projection::project_context(
        provider,
        self_id,
        state,
        kernel,
        client_id,
        cfg.route,
        cfg.name,
        messages.clone(),
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            // too_small failsafe / unknown-strategy config error — the model is
            // NOT called. Surface it as the send error (the seam already pushed
            // the context:too_small event).
            return Err(e
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("context projection error")
                .to_string());
        }
    };
    let tools = vec![send_tool_def()];
    let mut last_text;
    let mut iteration = 0usize;
    loop {
        iteration += 1;
        if iteration > 1 {
            emit_status(
                kernel,
                state,
                self_id,
                client_id,
                cfg.route,
                "thinking",
                Map::new(),
            )
            .await;
        }
        let (content, tool_calls) = run_pass(&ctx, &model_messages, &tools).await?;
        last_text = content;

        if tool_calls.is_empty() {
            break;
        }

        // Record the assistant turn with its tool_calls (to BOTH lists).
        let assistant_calls: Vec<Value> = tool_calls
            .iter()
            .map(|c| assistant_tool_call(c, cfg.tool_args_as_json))
            .collect();
        let assistant_turn = json!({
            "role": "assistant",
            "content": last_text,
            "tool_calls": assistant_calls,
        });
        model_messages.push(assistant_turn.clone());
        messages.push(assistant_turn);

        // Dispatch the batch (parallel or serial), order preserved.
        let results: Vec<Value> = if cfg.parallel_tools {
            let futures = tool_calls.iter().map(|c| dispatch_one(&ctx, c));
            futures_util::future::join_all(futures).await
        } else {
            let mut out = Vec::with_capacity(tool_calls.len());
            for c in &tool_calls {
                out.push(dispatch_one(&ctx, c).await);
            }
            out
        };

        // Menu invalidates AFTER each tool batch.
        *state.menu.lock().expect("menu poisoned") = None;
        model_messages.extend(results.clone());
        messages.extend(results);
    }

    // Done.
    emit_status(kernel, state, self_id, client_id, cfg.route, "done", {
        let mut m = Map::new();
        m.insert("reason".to_string(), json!("ok"));
        m
    })
    .await;
    emit_done(kernel, self_id, client_id, cfg.route).await;

    // Append final assistant turn to the FULL list + persist everything except the
    // rebuilt-each-turn system block at index 0 (the durable store is never trimmed
    // by projection — `messages` is the full conversation, not the model view).
    messages.push(json!({"role": "assistant", "content": last_text}));
    let to_persist: Vec<Value> = messages.iter().skip(1).cloned().collect();
    let _ = save_history(self_id, kernel, client_id, &to_persist).await;
    Ok(last_text)
}
