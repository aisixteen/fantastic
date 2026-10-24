//! Inline agent views in a chat (#339): `embed {id}` and `unembed {id}`.
//!
//! An embed is ONE appended store row, `{kind:"embed", content:<agent id>}` — it has
//! no `role`, so it is a row of the conversation's store but never a TURN: it does
//! not enter the model's prompt, projection or retrieval (`load_history` keeps
//! only rows with a chat role). `history` merges the card rows back into its
//! digests ([`crate::verbs::history`]) so a chat page redraws the same cards, in the
//! same places, after a reload or a restart. The store is append-only, so
//! `unembed` appends a tombstone row `{kind:"unembed", content:<id>}`; the cards a
//! chat shows are its embed rows since the last `clear` minus the tombstoned ones
//! ([`cards`]). A `context_clear` starts a new thread, so cards before it are not in
//! the head — the same as turns.
//!
//! Neither verb creates, moves or deletes an agent. `embed` makes the same checks
//! the canvas's `place {backend_id}` makes: the agent exists, it has a `view`
//! record, and the page may call what the panel calls (`view.calls` plus `watch`,
//! all granted to `user` in the agent's `callable_by`); it names what is missing as
//! `grants_missing`. The Rust side cannot measure pixels: it answers with the
//! card's box (`width:"100%"`, `max_height`), and the chat page, which draws the
//! panel, measures it and tells the model when it overflowed.

use crate::constants::CHAT_CARD_MAX_HEIGHT;
use crate::memory::{refresh_head, save_history, MemResult};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// The row kind that puts a card in the chat.
pub const EMBED_KIND: &str = "embed";
/// The row kind that takes it out again.
pub const UNEMBED_KIND: &str = "unembed";

/// The card rows of a conversation (full rows or `history` digests), as the agent
/// ids that are on show, oldest first: an embed adds its agent once, an unembed
/// removes it. The id is the row's `content` (a digest's `head`).
#[must_use]
pub fn cards(rows: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        let kind = r.get("kind").and_then(Value::as_str);
        if kind != Some(EMBED_KIND) && kind != Some(UNEMBED_KIND) {
            continue;
        }
        let Some(id) = r
            .get("content")
            .or_else(|| r.get("head"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        out.retain(|c| c != id);
        if kind == Some(EMBED_KIND) {
            out.push(id.to_string());
        }
    }
    out
}

/// Is `row` a card row (embed or unembed)?
#[must_use]
pub fn is_card_row(row: &Value) -> bool {
    matches!(
        row.get("kind").and_then(Value::as_str),
        Some(EMBED_KIND | UNEMBED_KIND)
    )
}

/// This agent's card rows in the head (since the last clear), oldest first.
pub async fn card_rows(self_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<Vec<Value>> {
    Ok(refresh_head(self_id, kernel)
        .await?
        .into_iter()
        .filter(is_card_row)
        .collect())
}

fn refusal(msg: String, reason: &str, hint: &str) -> Value {
    json!({"error": msg, "reason": reason, "arg": "id", "hint": hint})
}

fn id_arg(verb: &str, payload: &Value) -> Result<String, Value> {
    match payload.get("id").and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(json!({
            "error": format!("{verb}: `id` is required — the host agent's id"),
            "reason": "bad_arg",
            "arg": "id",
        })),
    }
}

const MAKE_HINT: &str = "create_agent a host agent with view {kind:html, bytes_file:<panel file>, \
    calls:[get_html, watch]} and callable_by {user:[get_html, watch]}, then embed it by id";

/// Does a `callable_by` grant value let `verb` through (`"*"`, or a list naming it)?
fn allows(grant: &Value, verb: &str) -> bool {
    grant.as_str() == Some("*")
        || grant.as_array().is_some_and(|a| {
            a.iter()
                .any(|v| v.as_str() == Some(verb) || v.as_str() == Some("*"))
        })
}

/// `embed` verb: store a card row for host agent `id`. See the module doc.
pub async fn embed(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> MemResult<Value> {
    let id = match id_arg("embed", payload) {
        Ok(i) => i,
        Err(e) => return Ok(e),
    };
    if id == agent_id.0 {
        return Ok(refusal(
            "embed: a chat cannot embed itself".to_string(),
            "bad_arg",
            "embed another agent's id",
        ));
    }
    let Some(rec) = kernel
        .agents
        .get(&AgentId::from(id.as_str()))
        .map(|e| Arc::clone(&e))
        .map(|a| a.record())
    else {
        return Ok(refusal(
            format!("embed: no agent {id:?}"),
            "not_found",
            "reflect the kernel for the agents there",
        ));
    };
    let Some(view) = rec.meta.get("view").filter(|v| v.is_object()) else {
        return Ok(refusal(
            format!("embed: {id} has no `view` record — a default chat/terminal/stub view is not an embeddable panel"),
            "no_view",
            MAKE_HINT,
        ));
    };
    // the verbs the panel calls: its record's `calls`, plus `watch` (always received)
    let mut calls: Vec<String> = view
        .get("calls")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !calls.iter().any(|c| c == "watch") {
        calls.push("watch".to_string());
    }
    let user = rec
        .meta
        .get("callable_by")
        .and_then(|c| c.get("user"))
        .cloned()
        .unwrap_or(Value::Null);
    let missing: Vec<&String> = calls.iter().filter(|c| !allows(&user, c)).collect();
    if !missing.is_empty() {
        let want: Vec<&String> = calls.iter().collect();
        let mut r = refusal(
            format!("embed: {id}'s callable_by does not let the user call {missing:?}"),
            "grants_missing",
            &format!(
                "the panel's calls will be refused. Grant them: update_agent {{id: {id}, callable_by: {{\"$self\":\"*\", \"user\": {}}}}}",
                json!(want)
            ),
        );
        r["grants_missing"] = json!({ id.clone(): missing });
        return Ok(r);
    }
    let live = cards(&card_rows(agent_id, kernel).await?);
    if live.contains(&id) {
        return Ok(json!({"embedded": id, "already": true, "card": card_box()}));
    }
    let ids = save_history(
        agent_id,
        kernel,
        &[json!({"kind": EMBED_KIND, "content": id})],
    )
    .await?;
    Ok(json!({
        "embedded": id,
        "row": ids.first(),
        "card": card_box(),
        "view": view.get("kind").cloned().unwrap_or(Value::Null),
        "hint": format!(
            "the card is {CHAT_CARD_MAX_HEIGHT} px tall at most and fills the chat column's width. \
             If the chat page finds it overflowing or taller than that it tells you — then redo \
             the panel. unembed {{id}} takes the card out; the agent stays."
        ),
    }))
}

/// `unembed` verb: take the card of agent `id` out of the chat (a tombstone row).
/// Works for a live and for a deleted agent; never reads or writes the agent.
pub async fn unembed(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> MemResult<Value> {
    let id = match id_arg("unembed", payload) {
        Ok(i) => i,
        Err(e) => return Ok(e),
    };
    if !cards(&card_rows(agent_id, kernel).await?).contains(&id) {
        return Ok(json!({"unembedded": id, "was_embedded": false}));
    }
    save_history(
        agent_id,
        kernel,
        &[json!({"kind": UNEMBED_KIND, "content": id})],
    )
    .await?;
    Ok(json!({"unembedded": id}))
}

/// The box a chat card gets: the column's whole width, at most the cap tall.
fn card_box() -> Value {
    json!({"width": "100%", "max_height": CHAT_CARD_MAX_HEIGHT})
}
