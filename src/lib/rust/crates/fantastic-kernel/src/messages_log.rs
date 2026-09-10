//! The message store on disk — cold hydration and the append pump.
//!
//! A port of python's `kernel_state/tools.py` (`read_messages`,
//! `hydrate_messages`, `_MessagePump`), which is canonical. The format is
//! [`crate::messages_xml`].
//!
//! THE KERNEL NEVER KNOWS DISK, here as there. It starts empty, always: this
//! module reads the log FROM ABOVE and fills the store with `rows.put`, and the
//! pump tails `rows.subscribe` and appends. Both are optional — no store wired
//! means the log stays in RAM, which is the same law records live under, not a
//! fallback.
//!
//! ORDER, BY CONTRACT: hydrate (cold, before any agent exists) → subscribe at
//! the highest id present → only then create children. Subscribing before
//! hydrating would re-append what was just read; creating children before
//! subscribing would lose their first messages. Rust gets the second half for
//! free — `subscribe(after)` REPLAYS everything above the cursor, so capturing
//! `after` at hydration time covers anything minted before the pump starts.

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::messages_xml::{parse_messages, row_to_xml, HEADER};
use crate::persistence::find_store;
use crate::rows::Row;
use crate::{AgentId, Kernel};

/// The log, under the workdir's `.fantastic/`.
pub const MESSAGES_FILE: &str = "messages.xml";

/// Debounce before a flush, matching python's pump: a burst of rows from one
/// turn becomes one append, not one per row.
const DEBOUNCE_MS: u64 = 150;

/// COLD HYDRATION of the message store — `(rows, next_id)`.
///
/// Read before any agent exists, like the record tree. A file of another
/// version, or a cut row, is a LOUD error — nothing is skipped or repaired.
pub fn read_messages(root_dir: &Path) -> Result<(Vec<Row>, i64), String> {
    let path = root_dir.join(MESSAGES_FILE);
    if !path.exists() {
        return Ok((Vec::new(), 1));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{MESSAGES_FILE}: {e}"))?;
    let head = text.lines().next().unwrap_or("").trim();
    if head != HEADER.trim() {
        return Err(format!(
            "{MESSAGES_FILE}: unknown log version — first line is {head:?}, this \
             loader writes {:?}",
            HEADER.trim()
        ));
    }
    let rows: Vec<Row> = parse_messages(&text)?
        .into_iter()
        .filter(|r| r.get("id").and_then(Value::as_i64).is_some())
        .collect();
    let top = rows
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .max()
        .unwrap_or(0);
    Ok((rows, top + 1))
}

/// Apply [`read_messages`] to a kernel FROM ABOVE, before its first mint.
///
/// Sets the counter FIRST, so every hydrated id is a pre-minted one. Returns the
/// row count. A duplicate id is the caller ingesting the same file twice and is
/// said loudly, exactly as `rows.put` says it.
pub fn hydrate_messages(kernel: &Kernel, root_dir: &Path) -> Result<usize, String> {
    let (rows, next_id) = read_messages(root_dir)?;
    kernel.rows.set_next_id(next_id);
    let mut n = 0;
    for row in rows {
        let agent = row
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{MESSAGES_FILE}: a row carries no `agent`"))?;
        kernel.rows.put(&AgentId::from(agent), row)?;
        n += 1;
    }
    Ok(n)
}

/// Tail the store and append every row to the log, forever.
///
/// `after` is the highest id hydration saw — the pump replays anything minted
/// between then and now, so nothing that landed while the roster booted is lost.
///
/// NO STORE WIRED MEANS RAM ONLY, and it says so once. That is the law records
/// live under too: the kernel does not require a disk to work, and a loader that
/// wires one is what makes anything durable.
pub fn install_message_pump(kernel: &Arc<Kernel>, after: i64) {
    let k = Arc::clone(kernel);
    tokio::spawn(async move {
        let mut tail = k.rows.subscribe(after);
        let mut said_no_store = false;
        loop {
            let Some(first) = tail.recv().await else {
                return;
            };
            // DEBOUNCE: give the rest of this turn's rows a moment to land, so
            // one turn is one append rather than one per row.
            tokio::time::sleep(std::time::Duration::from_millis(DEBOUNCE_MS)).await;
            let mut batch = vec![first];
            while let Ok(Some(id)) =
                tokio::time::timeout(std::time::Duration::from_millis(1), tail.recv()).await
            {
                batch.push(id);
            }
            let Some(store_id) = find_store(&k) else {
                if !said_no_store {
                    said_no_store = true;
                    println!("  [messages] no store wired — messages stay in RAM");
                }
                continue;
            };
            // READ THE ROW AT FLUSH TIME, not when it was queued: an outcome
            // stamped onto a `call` row inside the debounce window belongs in
            // the line we are about to write. That is why the tail carries ids.
            let mut lines = String::new();
            for id in batch {
                let Some(row) = k.rows.get_one(id) else {
                    continue;
                };
                if skips(&k, &store_id, &row) {
                    continue;
                }
                lines.push_str(&row_to_xml(&row));
            }
            if lines.is_empty() {
                continue;
            }
            if header_needed(&k, &store_id).await {
                lines.insert_str(0, HEADER);
            }
            let (reply, _) = crate::send::with_operator(k.send_with_binary(
                &store_id,
                json!({"type": "write_stream", "path": MESSAGES_FILE}),
                lines.into_bytes(),
            ))
            .await;
            if let Some(err) = reply.get("error") {
                println!("  [messages] append refused: {err}");
            }
        }
    });
}

/// Is the log absent (so the root must be written first)? Probed once per
/// append rather than remembered, because a store can be wired mid-run.
///
/// ON THE BINARY CHANNEL, like the write. `read_stream` is a binary verb: the
/// file bundle REFUSES it on the text path, so a plain send answers "error" for
/// a file that is sitting right there — and the root would be re-written at the
/// head of every append, in the middle of the document.
async fn header_needed(kernel: &Arc<Kernel>, store_id: &AgentId) -> bool {
    let (reply, _) = crate::send::with_operator(kernel.send_with_binary(
        store_id,
        json!({"type": "read_stream", "path": MESSAGES_FILE, "offset": 0, "length": 1}),
        Vec::new(),
    ))
    .await;
    reply.get("error").is_some()
}

/// THE PUMP SKIPS ITS OWN TRAFFIC. A flush is a send, so it mints a `call` row
/// on the store; persisting that row would mint another, forever.
fn skips(kernel: &Arc<Kernel>, store_id: &AgentId, row: &Row) -> bool {
    let _ = kernel;
    let kind = row.get("kind").and_then(Value::as_str);
    let agent = row.get("agent").and_then(Value::as_str);
    // The store's own OPERATOR-CONTEXT calls: the pump's writes and probes,
    // which carry no origin because nobody sent them.
    kind == Some("call") && agent == Some(store_id.0.as_str()) && row.get("origin").is_none()
}

#[cfg(test)]
#[path = "messages_log/tests.rs"]
mod tests;
