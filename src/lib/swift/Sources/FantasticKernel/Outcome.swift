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
    /// Who to call. `"kernel"` is the reserved address of the root.
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

    /// The RAW (tag-typed) `exec_id`, when a caller is FORWARDING a value it
    /// did not itself parse — a model's `<exec_id>` argument, or a wire
    /// frame's `id` field (C38). An untyped `JSON` the KERNEL validates and
    /// produces python's exact refusals for: an int NAMES an execution to
    /// JOIN (refused if none exists under it), a non-empty string NAMES one
    /// to open-or-join, anything else is `bad_arg`. Set, this SUPERSEDES the
    /// typed `id` above — routing used to coerce with `args["exec_id"]?
    /// .asString`, which silently DROPPED an int `exec_id` (never attempting
    /// the join python performs) instead of forwarding it.
    public var rawId: JSON?
    /// The RAW `timeout`, forwarded untouched (C38) instead of `args["timeout"]?
    /// .asDouble`, which silently DROPPED a malformed value (e.g. the string
    /// `"soon"`) — defaulting to "wait forever" — instead of the kernel's
    /// `bad_arg` refusal. Set, this SUPERSEDES the typed `timeout` above.
    public var rawTimeout: JSON?
    /// The RAW `retries`, forwarded untouched (C38) for the same reason. Set,
    /// this SUPERSEDES the typed `retries` above.
    public var rawRetries: JSON?

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
    /// THE BINARY REPLY BODY — the carrier for the `bytes` value python's reply
    /// dict holds natively (C37). `nil` = the verb returned NO chunk;
    /// `.some(d)` = it returned one, `d` possibly EMPTY (a 0-byte read of an
    /// empty file). Every wire leg picks its response shape from THAT
    /// distinction, exactly as python's `isinstance(reply["bytes"], bytes)`
    /// does; testing `isEmpty` instead served an empty file as JSON.
    ///
    /// Only a binary item can set it (`Item.binary`), and only the bundle's
    /// `handleBinary` fills it in.
    public var body: Data?

    public init(
        pid: TaskId?, status: Status, result: JSON, attempts: UInt32 = 1, body: Data? = nil
    ) {
        self.pid = pid
        self.status = status
        self.result = result
        self.attempts = attempts
        self.body = body
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
