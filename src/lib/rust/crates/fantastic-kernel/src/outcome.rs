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
pub use fantastic_bundle::path::Step;
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
    /// Not `Completed`: the durable row carries `error`/`reason` off the result,
    /// and beside `status: completed` one row would say both "fine" and
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

/// A RAW BODY and the slot it sits at — python's `bytes` value inside the
/// payload (or reply) dict, which a `serde_json::Value` cannot hold. The bytes
/// ride BESIDE the JSON (base64 is banned); the path says where python's dict
/// holds them, so a handler finds its body under its own key.
///
/// ONE SPELLING OF THE SLOT: a list of [`Step`]s — an object key taken
/// literally, a list index — exactly a binary frame's `_binary_path` below the
/// payload. The REST octet leg's `X-Fantastic-Bytes-Field: a.b` is the one key
/// `"a.b"` ([`Blob::key`]); a nested slot is `[meta, chunk]`. Every place the
/// kernel puts or takes the slot goes through [`Blob::place`] / [`Blob::take`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    /// Where the body sits, relative to the payload (or the reply):
    /// `[bytes]`, `[data]`, `[a.b]`, `[meta, chunk]`, `[items, 0]`.
    pub path: Vec<Step>,
    /// The raw body.
    pub bytes: Vec<u8>,
}

impl Blob {
    /// The body `bytes` at `path` (non-empty: a body is never the payload itself).
    #[must_use]
    pub fn new(path: Vec<Step>, bytes: Vec<u8>) -> Self {
        Self { path, bytes }
    }

    /// The body `bytes` under the ONE top-level key `key`, literally — python's
    /// `payload[field] = body`.
    #[must_use]
    pub fn key(key: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            path: vec![Step::Key(key.into())],
            bytes,
        }
    }

    /// The steps from the payload to the body's slot.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.path
    }

    /// Put `value` in the body's slot of `obj` —
    /// [`fantastic_bundle::path::place`]: a last object key is assigned (kept in
    /// place when present, appended when absent — python's dict assignment), a
    /// list slot replaced.
    ///
    /// # Errors
    /// The slot does not exist in `obj` (every step before the last must).
    pub fn place(&self, obj: &mut Value, value: Value) -> Result<(), String> {
        fantastic_bundle::path::place(obj, &self.path, value)
    }

    /// Take the body's slot out of `obj` — python's `del`: an object key is
    /// removed (the others keep their order), a list slot nulled
    /// ([`fantastic_bundle::path::take`]).
    ///
    /// # Errors
    /// The slot is not in `obj`.
    pub fn take(&self, obj: &mut Value) -> Result<(), String> {
        fantastic_bundle::path::take(obj, &self.path)
    }

    /// The bytes when they sit at the ONE top-level key `key` — python's
    /// `isinstance(payload.get(key), bytes)`: a handler asks for ITS key, and a
    /// body placed under another key (or nested) is not its body.
    #[must_use]
    pub fn at(&self, key: &str) -> Option<&[u8]> {
        matches!(self.path.as_slice(), [Step::Key(k)] if k == key).then_some(self.bytes.as_slice())
    }

    /// [`Blob::at`], owned: the bytes when they sit at the ONE top-level key
    /// `key`, else `None`.
    #[must_use]
    pub fn into_at(self, key: &str) -> Option<Vec<u8>> {
        self.at(key).is_some().then_some(self.bytes)
    }

    /// The same body one level DOWN: the path below its first step when that
    /// step is the key `key` (a frame's `payload` → the payload's own path).
    /// `None` when the body sits elsewhere, or IS that key's value.
    #[must_use]
    pub fn under(self, key: &str) -> Option<Self> {
        match self.path.split_first() {
            Some((Step::Key(k), rest)) if k == key && !rest.is_empty() => Some(Self {
                path: rest.to_vec(),
                bytes: self.bytes,
            }),
            _ => None,
        }
    }

    /// The same body one level UP, under the key `key` (the payload's path →
    /// the frame's `payload`).
    #[must_use]
    pub fn nested_in(&self, key: &str) -> Vec<Step> {
        let mut out = Vec::with_capacity(self.path.len() + 1);
        out.push(Step::Key(key.to_string()));
        out.extend(self.path.iter().cloned());
        out
    }
}

/// The body a binary dispatch carries at the top-level key `key`, if it carries
/// one there.
#[must_use]
pub fn body_at<'a>(body: Option<&'a Blob>, key: &str) -> Option<&'a [u8]> {
    body.and_then(|b| b.at(key))
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
    /// Optional RAW request body AND WHERE IT SITS in the payload — python's
    /// payload dict holding a `bytes` value at that path (`data` for
    /// `exec_bridge.paste_image`, `bytes` for `file_bridge.write_stream`).
    /// The handler gets it beside the header (`Bundle::handle_binary`, the ONE
    /// dispatch every item takes), under the same task row, ledger id, timeout
    /// and retries as any other item. Never base64: the bytes travel beside the
    /// JSON header, not inside it. A payload may name the slot (`null` there) to
    /// keep python's key order on the call row and the watcher frame; the
    /// handler gets the header without it.
    ///
    /// A REPLY body needs no request body and nothing declared: any verb may
    /// answer one ([`Outcome::body`]), as any python handler may return a dict
    /// holding bytes.
    pub binary: Option<Blob>,
    /// The RAW (tag-typed) `exec_id`, when a caller is forwarding a value it
    /// did not itself parse — a model's `<exec_id>` argument, or a wire
    /// frame's `id` field. An untyped [`Value`] the KERNEL validates
    /// and produces python's exact refusals for: `int(id)` NAMES an
    /// execution to JOIN (refused if none exists under it), a non-empty
    /// string NAMES one to open-or-join, anything else is `bad_arg`. Set,
    /// this SUPERSEDES the typed `id` above — coercing with
    /// `args.get("exec_id").and_then(Value::as_str)` would silently DROP an int
    /// `exec_id` (never attempting the join python performs) instead of
    /// forwarding it.
    pub raw_id: Option<Value>,
    /// The RAW `timeout`, forwarded untouched instead of
    /// `args.get("timeout").and_then(Value::as_f64)`, which silently
    /// DROPPED a malformed value (e.g. the string `"soon"`) — defaulting to
    /// "wait forever" — instead of the kernel's `bad_arg` refusal. Set, this
    /// SUPERSEDES the typed `timeout` above.
    pub raw_timeout: Option<Value>,
    /// The RAW `retries`, forwarded untouched for the same reason.
    /// Set, this SUPERSEDES the typed `retries` above.
    pub raw_retries: Option<Value>,
    /// SECRET MARKS (`crate::secret`): dotted paths into the payload whose values
    /// the kernel keeps and shows only as `[secret]`. RAW — the kernel validates.
    pub secret: Option<Value>,
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
            raw_id: None,
            raw_timeout: None,
            raw_retries: None,
            secret: None,
        }
    }

    /// Mark payload paths SECRET (`crate::secret`): the handler gets them as
    /// sent; every row, event and mirror shows them as `[secret]`.
    #[must_use]
    pub fn with_secret<S: Into<String>>(mut self, marks: impl IntoIterator<Item = S>) -> Self {
        self.secret = Some(Value::Array(
            marks.into_iter().map(|m| Value::from(m.into())).collect(),
        ));
        self
    }

    /// Forward a RAW `secret` field (a wire frame's) for the kernel to validate.
    #[must_use]
    pub fn with_raw_secret(mut self, v: Value) -> Self {
        self.secret = Some(v);
        self
    }

    /// Carry a RAW request body at `path` of the payload (its steps, relative to
    /// the payload: python's `set_path`, a binary frame's `_binary_path` below
    /// `payload`) — the handler gets it beside the header, as an ordinary
    /// execution of the batch (pid, call/settle rows, `ps` row, timeout, dedup,
    /// `<guid>|agent` routing). There is no second send path for bytes. A path
    /// that names no slot of the payload is refused before anything runs.
    #[must_use]
    pub fn with_binary(mut self, path: Vec<Step>, bytes: Vec<u8>) -> Self {
        self.binary = Some(Blob::new(path, bytes));
        self
    }

    /// Carry a RAW request body under the ONE top-level key `key`, literally —
    /// python's `payload[field] = body`: `a.b` names the key `"a.b"` (a nested
    /// `a.b` the payload already holds is left as sent). Otherwise
    /// [`Item::with_binary`].
    #[must_use]
    pub fn with_binary_key(mut self, key: impl Into<String>, bytes: Vec<u8>) -> Self {
        self.binary = Some(Blob::key(key, bytes));
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

    /// Forward a RAW (tag-typed) `exec_id` for the KERNEL to validate —
    /// use this instead of `.with_id(...)` whenever the value came from a
    /// caller this code did not itself construct (model tag args, a wire
    /// frame), so an int join or a malformed value reaches python's exact
    /// refusal instead of being silently coerced/dropped at the call site.
    #[must_use]
    pub fn with_raw_id(mut self, v: Value) -> Self {
        self.raw_id = Some(v);
        self
    }

    /// Forward a RAW `timeout` for the kernel to validate.
    #[must_use]
    pub fn with_raw_timeout(mut self, v: Value) -> Self {
        self.raw_timeout = Some(v);
        self
    }

    /// Forward RAW `retries` for the kernel to validate.
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
    /// reply dict holds natively. `None` = the verb returned NO chunk;
    /// `Some(b)` = it returned one, `b` possibly EMPTY (a 0-byte read of an
    /// empty file). Every wire leg picks its response shape from this
    /// distinction, exactly as python's `isinstance(reply["bytes"], bytes)`
    /// does; testing `is_empty()` instead served an empty file as JSON.
    ///
    /// ANY item can carry one back — no verb list, no flag on the call: a
    /// bundle's `handle_binary` returns it at its slot of the reply (`["bytes"]`
    /// for a `read_stream` chunk, `["chunk"]` for a bundle's own verb); a PEER's
    /// reply keeps the slot its frame named (`["data", "x.y"]` is the reply key
    /// `"x.y"`). The path is relative to `result`.
    pub body: Option<Blob>,
    /// REPLY MARKS (`crate::secret`): dotted paths into `result` the kernel
    /// records only as `[secret]` (`crate::ledger::stored_outcome`). `result`
    /// itself stays whole — it is the caller's answer.
    pub secret: Vec<String>,
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
            secret: Vec::new(),
        }
    }

    /// …carrying the binary reply body the handler returned, at its slot of the
    /// result.
    #[must_use]
    pub fn with_body(mut self, body: Option<Blob>) -> Self {
        self.body = body;
        self
    }

    /// …whose reply values at `marks` are recorded as `[secret]`.
    #[must_use]
    pub fn with_secret(mut self, marks: Vec<String>) -> Self {
        self.secret = marks;
        self
    }

    /// The reply body's raw bytes, wherever it sits.
    #[must_use]
    pub fn body_bytes(&self) -> Option<&[u8]> {
        self.body.as_ref().map(|b| b.bytes.as_slice())
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
