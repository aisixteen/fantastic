//! Event routing + the status phase machine.
//!
//! Per the LLM contract, stream events go to the originating caller ONLY. There
//! is ONE rule for how (python `_to_caller`): the `cli` caller round-trips
//! through the `cli` agent so its handler prints to stdout, and every other
//! caller gets the event on the BACKEND's inbox tagged with its `client_id` —
//! the browser's WS subscription mirrors that inbox and filters to its own id.
//! No fan-out: cli does not see browser tokens, and vice versa.

use crate::helpers::now_secs;
use crate::state::BackendState;
use crate::stream_parts::{Part, PartEvent, PartKind, PartSplitter};
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Route a streaming event to the originating caller's inbox — python
/// `_to_caller`, the ONE rule.
///
/// This used to be selected per backend by a `CallerRoute` knob: ollama
/// round-tripped `cli` and the other three always emitted on the agent's inbox.
/// Python has no such knob, so a `cli` caller on three of the four backends
/// never reached the `cli` handler and printed nothing.
pub async fn to_caller(kernel: &Arc<Kernel>, self_id: &AgentId, client_id: &str, mut ev: Value) {
    if let Value::Object(ref mut obj) = ev {
        obj.insert("client_id".to_string(), json!(client_id));
        obj.entry("source")
            .or_insert_with(|| Value::String(self_id.as_str().to_string()));
    }
    if client_id == crate::helpers::DEFAULT_CLIENT_ID {
        let _ = kernel
            .send(vec![Item::new(AgentId::from("cli"), ev)])
            .await
            .remove(0)
            .result;
    } else {
        // ON THE AGENT'S INBOX, tagged with `client_id` — not on an inbox named
        // after the client. A synthetic client id has no Agent, so it has no
        // watcher set, and a watcher that subscribed to the AGENT (which is what
        // a `watch` frame names) never saw these events at all: measured
        // 2026-09-10, `compaction:rust` fired its projection and pushed nothing
        // the harness could observe.
        kernel.emit(self_id, ev).await;
    }
}

/// Broadcast a structured `status` event AND update the current entry's
/// phase so the on-demand `status` verb stays in sync.
pub async fn emit_status(
    kernel: &Arc<Kernel>,
    state: &Arc<BackendState>,
    self_id: &AgentId,
    client_id: &str,
    phase: &str,
    extra_detail: Map<String, Value>,
) {
    let (send_id, started_at) = {
        let mut cur = state.current_meta.lock().expect("current poisoned");
        if let Some(c) = cur.as_mut() {
            c.phase = phase.to_string();
            (Some(c.send_id.clone()), Some(c.started_at))
        } else {
            (None, None)
        }
    };
    let queue_depth = state.queue.lock().expect("queue poisoned").len();
    let mut detail = extra_detail;
    if let Some(sid) = send_id {
        detail.entry("send_id").or_insert(json!(sid));
    }
    if let Some(t) = started_at {
        detail.entry("started_at").or_insert(json!(t));
    }
    detail
        .entry("queue_depth")
        .or_insert(json!(queue_depth as u64));
    let ev = json!({
        "type": "status",
        "source": self_id.as_str(),
        "phase": phase,
        "detail": Value::Object(detail),
        "ts": now_secs(),
    });
    to_caller(kernel, self_id, client_id, ev).await;
}

/// Routes part events ([`crate::stream_parts`]) to their addressees, deltas
/// BATCHED so a token-rate stream does not become a token-rate event stream —
/// python `_PartPublisher`.
///
/// A call part with an addressee goes to that inbox (each of `to`), as an emit —
/// gated like any emit on another agent; a refused one is dropped. Prose, thought
/// and calls without an addressee (done, await, inquiry_answer) go where the
/// tokens go: the caller ([`to_caller`]).
pub struct PartPublisher {
    kernel: Arc<Kernel>,
    self_id: AgentId,
    client_id: String,
    /// THE ANCHOR: the message this turn answers (python `re=current_task()`) —
    /// where a consumer places the draft in its own (possibly folded) history.
    re: Option<i64>,
    /// `(stream_id, part, text)` held, in insertion order (python's dict).
    pending: Vec<(String, Part, String)>,
    last: HashMap<String, Instant>,
}

impl PartPublisher {
    /// Deltas of one part are sent at most this often.
    pub const BATCH: Duration = Duration::from_millis(50);

    /// A publisher for one pass of `self_id`, answering `client_id`; `re` is the
    /// message this turn answers (`part_start` carries it).
    #[must_use]
    pub fn new(kernel: Arc<Kernel>, self_id: AgentId, client_id: String, re: Option<i64>) -> Self {
        Self {
            kernel,
            self_id,
            client_id,
            re,
            pending: Vec::new(),
            last: HashMap::new(),
        }
    }

    async fn send(&self, part: &Part, mut ev: Map<String, Value>) {
        ev.insert("stream_id".to_string(), json!(part.stream_id));
        ev.insert("source".to_string(), json!(self.self_id.as_str()));
        let addrs: Vec<String> = match (&part.to, &part.target) {
            (Some(to), _) if !to.is_empty() => to.clone(),
            (_, Some(t)) if !t.is_empty() => vec![t.clone()],
            _ => Vec::new(),
        };
        if part.kind != PartKind::Call || addrs.is_empty() {
            to_caller(
                &self.kernel,
                &self.self_id,
                &self.client_id,
                Value::Object(ev),
            )
            .await;
            return;
        }
        for addr in addrs {
            let target = if addr == "self" {
                self.self_id.clone()
            } else {
                AgentId::from(addr)
            };
            self.kernel.emit(&target, Value::Object(ev.clone())).await;
        }
    }

    async fn flush(&mut self, sid: &str) {
        let Some(at) = self.pending.iter().position(|(s, _, _)| s == sid) else {
            return;
        };
        let (_, part, text) = self.pending.remove(at);
        self.last.insert(sid.to_string(), Instant::now());
        let mut ev = Map::new();
        ev.insert("type".to_string(), json!("part_delta"));
        ev.insert("text".to_string(), json!(text));
        self.send(&part, ev).await;
    }

    /// Publish `events`; `flush_all` flushes every held delta (the pass is
    /// over) — python `push(events, final=True)`.
    pub async fn push(&mut self, events: Vec<PartEvent>, flush_all: bool) {
        for ev in events {
            let sid = ev.part().stream_id.clone();
            match ev {
                PartEvent::Start(part) => {
                    let mut m = Map::new();
                    m.insert("type".to_string(), json!("part_start"));
                    m.extend(part.head());
                    if let Some(re) = self.re {
                        m.insert("re".to_string(), json!(re));
                    }
                    self.send(&part, m).await;
                    self.last.insert(sid, Instant::now());
                }
                PartEvent::Delta(part, text) => {
                    match self.pending.iter_mut().find(|(s, _, _)| *s == sid) {
                        Some(held) => {
                            held.1 = part;
                            held.2.push_str(&text);
                        }
                        None => self.pending.push((sid.clone(), part, text)),
                    }
                    let due = match self.last.get(&sid) {
                        Some(t) => t.elapsed() >= Self::BATCH,
                        None => true,
                    };
                    if due {
                        self.flush(&sid).await;
                    }
                }
                PartEvent::End(part, ok) => {
                    self.flush(&sid).await;
                    let mut m = Map::new();
                    m.insert("type".to_string(), json!("part_end"));
                    if let Some(ok) = ok {
                        m.insert("ok".to_string(), json!(ok));
                    }
                    self.send(&part, m).await;
                }
                PartEvent::Abort(part, why) => {
                    self.pending.retain(|(s, _, _)| *s != sid);
                    let mut m = Map::new();
                    m.insert("type".to_string(), json!("part_abort"));
                    m.insert("reason".to_string(), json!(why));
                    self.send(&part, m).await;
                }
            }
        }
        if flush_all {
            let held: Vec<String> = self.pending.iter().map(|(s, _, _)| s.clone()).collect();
            for sid in held {
                self.flush(&sid).await;
            }
        }
    }
}

/// One pass's streaming parts — the splitter and its publisher, kept on the
/// in-flight entry (python `_current[id]["parts"]`) so `stop` can find a part and
/// `on_cancel` can abort the open ones.
pub struct PassParts {
    /// Splits the raw stream into parts.
    pub splitter: PartSplitter,
    /// Routes the parts' events.
    pub publisher: PartPublisher,
}

impl std::fmt::Debug for PassParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PassParts")
            .field("splitter", &self.splitter)
            .finish_non_exhaustive()
    }
}
