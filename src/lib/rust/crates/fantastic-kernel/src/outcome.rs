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
use serde_json::Value;

/// How one batch item ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The handler ran and returned an answer that is not an error; `result` is
    /// its reply verbatim.
    Completed,
    /// The handler RAN AND SAID NO: it returned a map carrying a truthy `error`.
    /// A semantic failure — a missing path, a patch that did not match, a
    /// refusal the handler composed itself. `result` is that reply, verbatim.
    ///
    /// This used to settle `Completed`, on the rule that the kernel does not
    /// introspect domain replies. The rule did not survive contact with its own
    /// storage: the durable row already carried `error`/`reason` copied off the
    /// result BESIDE `status: completed`, so one row said both "fine" and
    /// "failed". Failure is a STATUS; nothing downstream should have to parse a
    /// result to learn it.
    Errored,
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
            Status::Errored => "errored",
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
    /// Who to call. The reserved `"kernel"` address names the root.
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
    /// Optional RAW request body — python's `payload["bytes"]` holding bytes.
    /// Present ⇒ the item rides the BINARY channel (`Bundle::handle_binary`)
    /// instead of the text one, under the same task row, ledger id, timeout and
    /// retries as any other item. Never base64: the bytes travel beside the JSON
    /// header, not inside it. A payload may name the slot (`"bytes": null`) to
    /// keep python's key order on the call row and the watcher frame; the
    /// handler gets the header without it.
    pub binary: Option<Vec<u8>>,
    /// The caller reads the reply's chunk RAW ([`Outcome::body`]) — the item
    /// rides the binary channel with no request body. Python's reply dict holds
    /// `bytes` natively; a request that sends none (`read_stream`) is a plain
    /// dict on the call row and a text frame to a watcher.
    pub raw_reply: bool,
    /// The RAW (tag-typed) `exec_id`, when a caller is forwarding a value it
    /// did not itself parse — a model's `<exec_id>` argument, or a wire
    /// frame's `id` field (C38). An untyped [`Value`] the KERNEL validates
    /// and produces python's exact refusals for: `int(id)` NAMES an
    /// execution to JOIN (refused if none exists under it), a non-empty
    /// string NAMES one to open-or-join, anything else is `bad_arg`. Set,
    /// this SUPERSEDES the typed `id` above — routing used to coerce with
    /// `args.get("exec_id").and_then(Value::as_str)`, which silently
    /// DROPPED an int `exec_id` (never attempting the join python performs)
    /// instead of forwarding it.
    pub raw_id: Option<Value>,
    /// The RAW `timeout`, forwarded untouched (C38) instead of
    /// `args.get("timeout").and_then(Value::as_f64)`, which silently
    /// DROPPED a malformed value (e.g. the string `"soon"`) — defaulting to
    /// "wait forever" — instead of the kernel's `bad_arg` refusal. Set, this
    /// SUPERSEDES the typed `timeout` above.
    pub raw_timeout: Option<Value>,
    /// The RAW `retries`, forwarded untouched (C38) for the same reason.
    /// Set, this SUPERSEDES the typed `retries` above.
    pub raw_retries: Option<Value>,
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
            raw_reply: false,
            raw_id: None,
            raw_timeout: None,
            raw_retries: None,
        }
    }

    /// Carry a RAW request body — the item dispatches through the binary
    /// channel, as an ordinary execution of the batch (pid, call/settle rows,
    /// `ps` row, timeout, dedup, `<guid>|agent` routing). There is no second
    /// send path for bytes.
    #[must_use]
    pub fn with_binary(mut self, blob: Vec<u8>) -> Self {
        self.binary = Some(blob);
        self
    }

    /// Take the reply's chunk RAW ([`Outcome::body`]) without sending one — a
    /// `read_stream`. See [`Item::raw_reply`].
    #[must_use]
    pub fn with_raw_reply(mut self) -> Self {
        self.raw_reply = true;
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

    /// Forward a RAW (tag-typed) `exec_id` for the KERNEL to validate (C38) —
    /// use this instead of `.with_id(...)` whenever the value came from a
    /// caller this code did not itself construct (model tag args, a wire
    /// frame), so an int join or a malformed value reaches python's exact
    /// refusal instead of being silently coerced/dropped at the call site.
    #[must_use]
    pub fn with_raw_id(mut self, v: Value) -> Self {
        self.raw_id = Some(v);
        self
    }

    /// Forward a RAW `timeout` for the kernel to validate (C38).
    #[must_use]
    pub fn with_raw_timeout(mut self, v: Value) -> Self {
        self.raw_timeout = Some(v);
        self
    }

    /// Forward RAW `retries` for the kernel to validate (C38).
    #[must_use]
    pub fn with_raw_retries(mut self, v: Value) -> Self {
        self.raw_retries = Some(v);
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
    /// THE BINARY REPLY BODY — the carrier for the `bytes` value python's
    /// reply dict holds natively (C37). `None` = the verb returned NO chunk;
    /// `Some(b)` = it returned one, `b` possibly EMPTY (a 0-byte read of an
    /// empty file). Every wire leg picks its response shape from this
    /// distinction, exactly as python's `isinstance(reply["bytes"], bytes)`
    /// does; testing `is_empty()` instead served an empty file as JSON.
    ///
    /// Only a binary item can set it (`Item::with_binary` /
    /// `Item::with_raw_reply`), and only the bundle's `handle_binary` fills it.
    pub body: Option<Vec<u8>>,
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
            body: None,
        }
    }

    /// …carrying the binary reply body the handler returned (C37).
    #[must_use]
    pub fn with_body(mut self, body: Option<Vec<u8>>) -> Self {
        self.body = body;
        self
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
        crate::jsonsafe::obj([
            ("pid", Value::from(self.pid.as_ref().map(|p| p.0))),
            ("status", Value::from(self.status.as_str())),
            ("result", self.result.clone()),
            ("attempts", Value::from(self.attempts)),
        ])
    }
}

#[cfg(test)]
#[path = "outcome/tests.rs"]
mod tests;
