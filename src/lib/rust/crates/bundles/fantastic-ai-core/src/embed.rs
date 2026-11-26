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
use crate::memory::MemResult;
use fantastic_kernel::bundle::BundleError;
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

/// The kernel's refusal for the `id` argument, with what to do instead.
fn refused(code: &str, msg: String, hint: &str) -> Value {
    let mut r = refusal_hint(code, msg, hint);
    with(&mut r, "arg", json!("id"));
    r
}

const MAKE_HINT: &str = "create_agent a host agent with view {kind:html, bytes_file:<panel file>, \
    calls:[get_html, watch]}, then embed it by id";

/// The cards on show, from the head's card rows (since the last clear), read by a
/// step (#795).
async fn on_show<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<Vec<String>>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    crate::steps::refreshed(agent_id, kernel, rec, move |rec, head| async move {
        let live = head.map(|h| cards(&h.into_iter().filter(is_card_row).collect::<Vec<_>>()));
        next(rec, live).await;
    })
    .await;
}

/// `embed` verb: store a card row for host agent `id`. See the module doc. The head's
/// cards and the row's append are steps (#795); `settle` answers the call.
pub(crate) async fn embed(agent_id: &AgentId, id: &str, kernel: &Arc<Kernel>, settle: crate::steps::Settle) {
    let id = id.to_string();
    if id == agent_id.0 {
        return settle.with(Ok::<_, BundleError>(refused(
            BAD_ARG,
            "embed: a chat cannot embed itself".to_string(),
            "embed another agent's id",
        )));
    }
    let Some(rec) = kernel.resolve_target(id.as_str())
        .map(|a| a.record())
    else {
        return settle.with(Ok::<_, BundleError>(refused(
            NOT_FOUND,
            format!("embed: no agent {id:?}"),
            "reflect the kernel for the agents there",
        )));
    };
    let Some(view) = rec.meta.get("view").filter(|v| v.is_object()).cloned() else {
        return settle.with(Ok::<_, BundleError>(refused(
            BAD_ARG,
            format!("embed: {id} has no `view` record — a default chat/terminal/stub view is not an embeddable panel"),
            MAKE_HINT,
        )));
    };
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    on_show(agent_id, kernel, settle, move |settle, live| async move {
        let live = match live {
            Ok(l) => l,
            Err(e) => return settle.with(Err::<Value, _>(e)),
        };
        if live.contains(&id) {
            return settle.with(Ok::<_, BundleError>(
                json!({"embedded": id, "already": true, "card": card_box()}),
            ));
        }
        let row = json!({"kind": EMBED_KIND, "content": id});
        crate::steps::append(&me, &k, &[row], settle, move |settle, ids| async move {
            settle.with(ids.map(|ids| {
                json!({
                    "embedded": id,
                    "row": ids.first(),
                    "card": card_box(),
                    "view": view.get("kind").cloned().unwrap_or(Value::Null),
                    "hint": format!(
                        "the card is {CHAT_CARD_MAX_HEIGHT} px tall at most and fills the chat column's width. \
                         If the chat page finds it overflowing or taller than that it tells you — then redo \
                         the panel. unembed {{id}} takes the card out; the agent stays."
                    ),
                })
            }));
        })
        .await;
    })
    .await;
}

/// `unembed` verb: take the card of agent `id` out of the chat (a tombstone row).
/// Works for a live and for a deleted agent; never reads or writes the agent.
pub(crate) async fn unembed(agent_id: &AgentId, id: &str, kernel: &Arc<Kernel>, settle: crate::steps::Settle) {
    let id = id.trim().to_string();
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    on_show(agent_id, kernel, settle, move |settle, live| async move {
        match live {
            Err(e) => settle.with(Err::<Value, _>(e)),
            Ok(live) if !live.contains(&id) => settle.with(Ok::<_, BundleError>(
                json!({"unembedded": id, "was_embedded": false}),
            )),
            Ok(_) => {
                let row = json!({"kind": UNEMBED_KIND, "content": id});
                crate::steps::append(&me, &k, &[row], settle, move |settle, r| async move {
                    settle.with(r.map(|_| json!({"unembedded": id})));
                })
                .await;
            }
        }
    })
    .await;
}

/// The box a chat card gets: the column's whole width, at most the cap tall.
fn card_box() -> Value {
    json!({"width": "100%", "max_height": CHAT_CARD_MAX_HEIGHT})
}
