// The batch-send vocabulary: one `Item` in, one `Outcome` out.
//
// Port of python `kernel/_kernel.py`'s dispatch-outcome block (canonical).
//
// The terminal states of ONE batch item, as `send` reports them. The task table
// itself stays LIVE-ONLY (a row is forgotten the moment its task ends); an
// outcome is returned to the caller, and — with the execution ledger — recorded
// under its id so a re-send returns it instead of running the handler again.
//
// - `completed` — the handler ran and returned; `result` is its reply VERBATIM.
//   May be null; may be a domain `{error:…}` — the kernel does not introspect
//   domain replies.
// - `failed` — no handler answer exists: refused before dispatch (gate denial,
//   unknown agent, malformed item) or the handler threw (`raised: true`).
// - `cancelled` — killed via `cancel` while the batch awaited it; `result` is
//   `{"cancelled": true}`.
// - `timed_out` — the item's own deadline expired and the kernel killed it
//   through the front door, so `onCancel` hooks ran; `result` is a `timeout`
//   refusal.
//
// INVARIANT: `pid == nil` ⟺ nothing was ever in the task table (refusals,
// malformed items).

import FantasticJSON
import Foundation

/// How one batch item ended.
public enum Status: String, Sendable {
    /// The handler ran and returned an answer that is not an error; `result` is
    /// its reply verbatim.
    case completed
    /// The handler RAN AND SAID NO: it returned an object carrying a truthy
    /// `error`. A semantic failure — a missing path, a patch that did not match,
    /// a refusal the handler composed itself. `result` is that reply, verbatim.
    ///
    /// This used to settle `completed`, on the rule that the kernel does not
    /// introspect domain replies. The rule did not survive contact with its own
    /// storage: the durable row already carried the error copied off the result
    /// beside `status: completed`, so one row said both "fine" and "failed".
    /// Failure is a STATUS; nothing downstream should have to parse a result to
    /// learn it.
    case errored
    /// No handler answer exists — refused before dispatch, or the handler threw.
    case failed
    /// Killed via `cancel` while the batch awaited it.
    case cancelled
    /// The item's own deadline expired; killed through the front door.
    case timedOut = "timed_out"

    /// Terminal-but-not-successful: the two states the retry engine may re-spend
    /// and the two that make a row a DEAD LETTER. `cancelled` is deliberately
    /// absent — an operator kill is a decision, not a transient.
    public var isRetryable: Bool { self == .failed || self == .timedOut }
}

/// ONE item of a batch send.
///
/// `id` NAMES the execution (ensure-semantics — see the ledger); `timeout` is
/// seconds and the kernel NEVER defaults it (absent = wait forever); `retries`
/// is a caller-declared budget the kernel never defaults either.
public struct Item: Sendable {
    /// Who to call. `"kernel"` aliases the root.
    public var target: AgentId
    /// The verb payload — `type` names the verb.
    public var payload: JSON
    /// Optional caller-chosen execution id. May not contain `:` (the
    /// minted-pid namespace).
    public var id: String?
    /// Optional per-item deadline in seconds. Absent = wait forever.
    public var timeout: Double?
    /// Optional retry budget for terminal failed/timed-out attempts.
    public var retries: UInt32?
    /// Optional RAW request body. Present ⇒ the item rides the BINARY channel
    /// (`AgentBundle.handleBinary`) instead of the text one, under the same task
    /// row, ledger id, timeout and retries as any other item. Never base64: the
    /// bytes travel beside the JSON header, not inside it. `sendWithBinary`
    /// bypassed the batch entirely, so such a call had no task row, no ledger id
    /// and no timeout (measured 2026-09-02).
    public var binary: Data?

    /// A plain item: call `target` with `payload`, wait forever, no retries.
    public init(
        _ target: AgentId, _ payload: JSON,
        id: String? = nil, timeout: Double? = nil, retries: UInt32? = nil,
        binary: Data? = nil
    ) {
        self.target = target
        self.payload = payload
        self.id = id
        self.timeout = timeout
        self.retries = retries
        self.binary = binary
    }

    /// Read an item off a wire frame's fields.
    ///
    /// The frame `id` IS the execution id — ONE id space, deliberately: id reuse
    /// across sockets or reconnects is the same intent, so a double-click
    /// collapses to one execution instead of two.
    public static func fromFrame(_ target: AgentId, _ payload: JSON, frameId: String?) -> Item {
        var it = Item(target, payload)
        if let f = frameId, !f.isEmpty { it.id = f }
        return it
    }
}

/// How ONE batch item settled.
public struct Outcome: Sendable {
    /// The task id, or `nil` when the item never reached the task table.
    public var pid: TaskId?
    /// Which terminal state.
    public var status: Status
    /// The handler's reply, or the refusal, verbatim.
    public var result: JSON
    /// How many dispatch attempts this execution spent (1 unless retried).
    public var attempts: UInt32

    public init(pid: TaskId?, status: Status, result: JSON, attempts: UInt32 = 1) {
        self.pid = pid
        self.status = status
        self.result = result
        self.attempts = attempts
    }

    /// The item never reached the task table: a gate denial, an unknown agent, a
    /// malformed item. The refusal rides in `result` UNCHANGED, so every
    /// caller's existing `error`/`reason` branch keeps working.
    public static func refused(_ result: JSON) -> Outcome {
        Outcome(pid: nil, status: .failed, result: result)
    }

    /// True when the handler ran and answered.
    public var isCompleted: Bool { status == .completed }

    /// The wire shape: `{pid, status, result, attempts}`.
    public var asJSON: JSON {
        .object([
            "pid": pid.map { JSON.integer($0.value) } ?? .null,
            "status": .string(status.rawValue),
            "result": result,
            "attempts": .integer(Int64(attempts)),
        ])
    }
}
