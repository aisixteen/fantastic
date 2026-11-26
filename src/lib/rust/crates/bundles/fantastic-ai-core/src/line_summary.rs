//! THE LINE SUMMARY (#709): one short line in which the model says what it is doing
//! now, offered only when the chat's record sets [`FIELD`].
//!
//! The model writes `<call name="line_summary"><text>…</text></call>` in its answer, in
//! our call grammar. The chat handles it itself: no agent is called and nothing is
//! stored but the answer row the call is written in, so the line stays in the history
//! like any call. The latest line is the chat's current line, kept as its state
//! ([`crate::state::BackendState::line_summary`]), read back from the history at boot,
//! returned by `reflect` as `line_summary {text, ts}`, and published as
//! [`EVENT`] `{text, ts}` on the chat's own inbox whenever a new one is written.
//!
//! ONE CONCEPT WITH THE FOLD TITLE: a stretch's latest line becomes its fold's title
//! when that stretch folds, and the summariser then writes only the body
//! ([`crate::strategies::fold::summarize`]); a stretch with no line keeps the
//! summariser's title. The line is cleaned the way a fold title is
//! ([`crate::folds::clean_title`]), so the view, `reflect` and the fold show the same
//! line.

use std::sync::Arc;

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use crate::memory::{rows_of, store_call, MemResult};
use crate::state::state_for;
use crate::tool_parse::{extract_tool_calls, ToolCall};

/// The record option that offers the call (off by default).
pub const FIELD: &str = "line_summaries";
/// The call's name in the grammar.
pub const CALL: &str = "line_summary";
/// The event a new line publishes on the chat's own inbox.
pub const EVENT: &str = "line_summary_changed";

/// How many rows a boot reads per page while it looks for the last line.
const PAGE: u64 = 64;

/// Whether this record offers the call.
#[must_use]
pub fn offered(meta: &Map<String, Value>) -> bool {
    meta.get(FIELD).and_then(Value::as_bool) == Some(true)
}

/// The line as it is kept and shown: trimmed, one line, capped like a fold title.
/// `None` when nothing is left.
#[must_use]
pub fn clean(raw: &str) -> Option<String> {
    let squashed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!squashed.is_empty()).then(|| crate::folds::clean_title(&squashed))
}

/// The line a parsed `line_summary` call carries, cleaned.
fn line_of(call: &ToolCall) -> Option<String> {
    call.arguments
        .get("text")
        .and_then(Value::as_str)
        .and_then(clean)
}

/// The last line an answer's text writes, if any.
#[must_use]
pub fn last_in(text: &str) -> Option<String> {
    extract_tool_calls(text)
        .iter()
        .filter(|c| c.name == CALL)
        .filter_map(line_of)
        .next_back()
}

/// The latest line among the assistant rows `rows[idxs]` — the title of the stretch
/// they are when it folds. `None` when the record does not offer the call, or no row
/// of the stretch writes a line.
#[must_use]
pub fn of_span(
    agent_id: &AgentId,
    kernel: &Kernel,
    rows: &[Value],
    idxs: &[usize],
) -> Option<String> {
    if !offered(&crate::helpers::agent_meta(agent_id, kernel)) {
        return None;
    }
    idxs.iter()
        .map(|&i| &rows[i])
        .filter(|r| crate::folds::row_role(r) == "assistant")
        .filter_map(|r| last_in(crate::folds::row_content(r)))
        .next_back()
}

/// The chat's current line, `{text, ts}`, or null when it has none.
#[must_use]
pub fn current(agent_id: &AgentId) -> Value {
    state_for(agent_id)
        .line_summary
        .lock()
        .expect("line_summary poisoned")
        .as_ref()
        .map_or(Value::Null, |(text, ts)| json!({"text": text, "ts": ts}))
}

/// `reflect` with the chat's `line_summary` property, when its record offers the call.
#[must_use]
pub fn with_line(agent_id: &AgentId, kernel: &Kernel, mut reflected: Value) -> Value {
    if offered(&crate::helpers::agent_meta(agent_id, kernel)) {
        if let Value::Object(m) = &mut reflected {
            m.insert(CALL.to_string(), current(agent_id));
        }
    }
    reflected
}

/// Keep `text` as the chat's current line and publish it on the chat's own inbox.
async fn set(agent_id: &AgentId, kernel: &Arc<Kernel>, text: String, ts: f64) {
    *state_for(agent_id)
        .line_summary
        .lock()
        .expect("line_summary poisoned") = Some((text.clone(), ts));
    kernel
        .emit(
            agent_id,
            json!({"type": EVENT, "source": agent_id.as_str(), "text": text, "ts": ts}),
        )
        .await;
}

/// The model's `line_summary` call, from its answer: kept and published. `Some` is
/// what the model is told instead: the record does not offer the call, or the call
/// holds no line.
pub async fn take(kernel: &Arc<Kernel>, agent_id: &AgentId, call: &ToolCall) -> Option<String> {
    if !offered(&crate::helpers::agent_meta(agent_id, kernel)) {
        return Some(format!(
            "line_summary: this chat does not offer it — its record leaves `{FIELD}` off"
        ));
    }
    let Some(text) = line_of(call) else {
        return Some(
            "line_summary: it takes `<text>`, one short line saying what you are doing now"
                .to_string(),
        );
    };
    set(agent_id, kernel, text, crate::helpers::now_secs()).await;
    None
}

/// AT BOOT: the chat's current line is the last `line_summary` its history holds,
/// with that row's time; published like a new one so a follower that missed the
/// restart has it. Read newest first, a page at a time, from the store — the head is
/// not loaded, so the boot stays lazy for the conversation. A record that does not
/// offer the call keeps no line.
///
/// # Errors
/// The memory store refused a read.
pub async fn restore(agent_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<()> {
    *state_for(agent_id)
        .line_summary
        .lock()
        .expect("line_summary poisoned") = None;
    if !offered(&crate::helpers::agent_meta(agent_id, kernel)) {
        return Ok(());
    }
    let mut before: Option<i64> = None;
    loop {
        let mut ask = json!({
            "type": "read",
            "match": {"role": "assistant"},
            "order": "desc",
            "limit": PAGE,
        });
        if let Some(b) = before {
            ask["before"] = json!(b);
        }
        let rows = rows_of(&store_call(agent_id, kernel, ask).await?, agent_id);
        for r in &rows {
            if let Some(text) = last_in(crate::folds::row_content(r)) {
                // every stored row carries its `ts` (the store stamps it)
                let ts = r
                    .get("attributes")
                    .and_then(|a| a.get("ts"))
                    .and_then(Value::as_f64)
                    .ok_or_else(|| {
                        crate::memory::MemoryStoreError(format!(
                            "row {:?} holds a line_summary but no attributes.ts",
                            crate::folds::row_id(r)
                        ))
                    })?;
                set(agent_id, kernel, text, ts).await;
                return Ok(());
            }
        }
        match rows.last().and_then(crate::folds::row_id) {
            Some(oldest) if rows.len() as u64 == PAGE => before = Some(oldest),
            _ => return Ok(()),
        }
    }
}
