//! The batch-send vocabulary: one [`Item`] in, one [`Outcome`] out.
//!
//! Port of python `kernel/_kernel.py`'s dispatch-outcome block (canonical).
//!
//! The terminal states of ONE batch item, as `send` reports them. The task table
//! itself stays LIVE-ONLY (a row is forgotten the moment its task ends); an
//! outcome is returned to the caller, and — with the execution ledger — recorded
//! under its id so a re-send returns it instead of running the handler again.
//!
//! - `completed` — the target's handler ran and returned; `result` is its reply
//!   VERBATIM. May be null; may be a domain `{"error": …}` — the kernel does not
//!   introspect domain replies.
//! - `failed` — no handler answer exists: the kernel refused before dispatch
//!   (gate denial, unknown agent, malformed item) or the handler panicked
//!   (`{"error": …, "raised": true}`).
//! - `cancelled` — the dispatch was killed via `cancel` while the batch awaited
//!   it; `result` is `{"cancelled": true}`.
//! - `timed_out` — the item's own deadline expired and the kernel killed it
//!   through the front door (`cancel_task`, so `on_cancel` hooks ran); `result`
//!   is a `timeout` refusal.
//!
//! INVARIANT: `pid.is_none()` ⟺ nothing was ever in the task table (refusals,
//! malformed items, the kernel-reflect fast path).

use crate::agent::AgentId;
use crate::tasks::TaskId;
use serde_json::{json, Value};

/// How one batch item ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The handler ran and returned; `result` is its reply verbatim.
    Completed,
    /// No handler answer exists — refused before dispatch, or the handler panicked.
    Failed,
    /// Killed via `cancel` while the batch awaited it.
    Cancelled,
    /// The item's own deadline expired; killed through the front door.
    TimedOut,
}

impl Status {
    /// The wire spelling. These strings ARE the contract — every runtime and
    /// every client branches on them.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::TimedOut => "timed_out",
        }
    }

    /// Terminal-but-not-successful: the two states the retry engine may respend
    /// and the two that make a row a DEAD LETTER. `cancelled` is deliberately
    /// absent — an operator kill is an absorbing decision, not a transient.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, Status::Failed | Status::TimedOut)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// ONE item of a batch send.
///
/// `id` NAMES the execution (ensure-semantics — see the ledger); `timeout` is
/// seconds and the kernel NEVER defaults it (absent = wait forever); `retries`
/// is a caller-declared budget the kernel never defaults either.
#[derive(Debug, Clone)]
pub struct Item {
    /// Who to call. `"kernel"` aliases the root.
    pub target: AgentId,
    /// The verb payload — `type` names the verb.
    pub payload: Value,
    /// Optional caller-chosen execution id. May not contain `:` (the
    /// minted-pid namespace).
    pub id: Option<String>,
    /// Optional per-item deadline in seconds. Absent = wait forever.
    pub timeout: Option<f64>,
    /// Optional retry budget for terminal failed/timed_out attempts.
    pub retries: Option<u32>,
    /// Optional RAW request body. Present ⇒ the item rides the BINARY channel
    /// (`Bundle::handle_binary`) instead of the text one, under the same task
    /// row, ledger id, timeout and retries as any other item. Never base64:
    /// the bytes travel beside the JSON header, not inside it.
    pub binary: Option<Vec<u8>>,
}

impl Item {
    /// A plain item: call `target` with `payload`, wait forever, no retries.
    #[must_use]
    pub fn new(target: impl Into<AgentId>, payload: Value) -> Self {
        Self {
            target: target.into(),
            payload,
            id: None,
            timeout: None,
            retries: None,
            binary: None,
        }
    }

    /// Carry a RAW request body — the item dispatches through the binary
    /// channel. `send_with_binary` bypassed the batch entirely, so such a call
    /// had no task row, no ledger id and no timeout (measured 2026-09-02).
    #[must_use]
    pub fn with_binary(mut self, blob: Vec<u8>) -> Self {
        self.binary = Some(blob);
        self
    }

    /// NAME this execution — the dedup/join/retry handle, and the same id
    /// `ps`/`cancel`/reply tags carry.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Bound THIS item's wait, in seconds.
    #[must_use]
    pub fn with_timeout(mut self, secs: f64) -> Self {
        self.timeout = Some(secs);
        self
    }

    /// Declare a retry budget for terminal failed/timed_out attempts.
    #[must_use]
    pub fn with_retries(mut self, n: u32) -> Self {
        self.retries = Some(n);
        self
    }

    /// Read an item off a wire frame's fields. `id` here is the FRAME id — the
    /// execution id and the correlation id are ONE id space, deliberately: id
    /// reuse across sockets or reconnects is the same intent, so a double-click
    /// collapses to one execution instead of two.
    #[must_use]
    pub fn from_frame(target: impl Into<AgentId>, payload: Value, frame_id: Option<&str>) -> Self {
        let mut it = Self::new(target, payload);
        if let Some(fid) = frame_id {
            if !fid.is_empty() {
                it.id = Some(fid.to_string());
            }
        }
        it
    }
}

/// How ONE batch item settled.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The task id, or `None` when the item never reached the task table.
    pub pid: Option<TaskId>,
    /// Which terminal state.
    pub status: Status,
    /// The handler's reply, or the refusal, verbatim.
    pub result: Value,
    /// How many dispatch attempts this execution spent (1 unless retried).
    pub attempts: u32,
}

impl Outcome {
    /// Settle with a pid (the item reached the task table).
    #[must_use]
    pub fn new(pid: Option<TaskId>, status: Status, result: Value) -> Self {
        Self {
            pid,
            status,
            result,
            attempts: 1,
        }
    }

    /// The item never reached the task table: a gate denial, an unknown agent,
    /// a malformed item. The refusal rides in `result` UNCHANGED, so every
    /// caller's existing `error`/`reason` branch keeps working.
    #[must_use]
    pub fn refused(result: Value) -> Self {
        Self::new(None, Status::Failed, result)
    }

    /// True when the handler ran and answered.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.status == Status::Completed
    }

    /// The wire shape: `{pid, status, result, attempts}`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "pid": self.pid.as_ref().map(|p| p.0.clone()),
            "status": self.status.as_str(),
            "result": self.result,
            "attempts": self.attempts,
        })
    }
}

#[cfg(test)]
#[path = "outcome/tests.rs"]
mod tests;
