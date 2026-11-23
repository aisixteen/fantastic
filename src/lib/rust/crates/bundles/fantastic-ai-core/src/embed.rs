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
//! the canvas's `place {backend_id}` makes: the agent exists and it has a `view`
//! record. Whether the page may call what the panel calls is the page leg's own
//! lines (grant model v9: the target keeps no list of callers), judged when it calls.
//! The Rust side cannot measure pixels: it answers with the
//! card's box (`width:"100%"`, `max_height`), and the chat page, which draws the
//! panel, measures it and tells the model when it overflowed.

use crate::constants::CHAT_CARD_MAX_HEIGHT;
use crate::memory::{refresh_head, save_history, MemResult};
use fantastic_kernel::refusal::{refusal_hint, with, BAD_ARG, NOT_FOUND};
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

/// The kernel's refusal for the `id` argument, with what to do instead.
fn refused(code: &str, msg: String, hint: &str) -> Value {
    let mut r = refusal_hint(code, msg, hint);
    with(&mut r, "arg", json!("id"));
    r
}

const MAKE_HINT: &str = "create_agent a host agent with view {kind:html, bytes_file:<panel file>, \
    calls:[get_html, watch]}, then embed it by id";

/// `embed` verb: store a card row for host agent `id`. See the module doc.
pub async fn embed(agent_id: &AgentId, id: &str, kernel: &Arc<Kernel>) -> MemResult<Value> {
    let id = id.to_string();
    if id == agent_id.0 {
        return Ok(refused(
            BAD_ARG,
            "embed: a chat cannot embed itself".to_string(),
            "embed another agent's id",
        ));
    }
    let Some(rec) = kernel.resolve_target(id.as_str())
        .map(|a| a.record())
    else {
        return Ok(refused(
            NOT_FOUND,
            format!("embed: no agent {id:?}"),
            "reflect the kernel for the agents there",
        ));
    };
    let Some(view) = rec.meta.get("view").filter(|v| v.is_object()) else {
        return Ok(refused(
            BAD_ARG,
            format!("embed: {id} has no `view` record — a default chat/terminal/stub view is not an embeddable panel"),
            MAKE_HINT,
        ));
    };
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
pub async fn unembed(agent_id: &AgentId, id: &str, kernel: &Arc<Kernel>) -> MemResult<Value> {
    let id = id.trim().to_string();
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
