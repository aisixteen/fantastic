// OPEN INQUIRIES (owner, 2026-10-06: fully async). An `inquiry` an AI received is
// accepted at once and runs as its own chain `(receiver, client_id)`; the answer the
// model gives in ANY turn of that chain — or the news that it ended without one —
// is delivered to the asker as a later message under the inquiry's pid. Nothing
// waits on it. Python `_open_inquiries` / `_awaiting` / `_close_inquiry`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// What an inquiry carries ON ITSELF (owner, 2026-10-06): that it IS one and how it
/// is answered. Delivered bare as `[from ai]` + the question, the reviewer in
/// peer_review (rust, twice) did the work, wrote `REJECT — timeout = 300` as prose
/// and ended with `done`; the one line in the system block was too far away.
/// Python `_INQUIRY_HEAD`.
func inquiryHead(asker: String, pid: String) -> String {
    "[inquiry from \(asker) · re \(pid) — do any work it needs, then answer with "
        + "<call name=\"inquiry_answer\"><answer>…</answer></call> before done: nothing else "
        + "counts as your answer]"
}

/// An `inquiry` an AI received and has not answered yet: the asker (`nil` for the
/// operator), the pid its reply tag carries, the asker's own thread, and the
/// request id.
struct OpenInquiry {
    let asker: String?
    let pid: String
    let replyClient: String
    let requestId: JSON
}

/// The two tables, KEYED BY KERNEL like `AISilentStreaks`: swift hosts many
/// kernels at once (every test does).
enum AIInquiries {
    private struct Chain: Hashable {
        let agent: String
        let client: String
    }
    private static let lock = NSLock()
    /// Open inquiries by the receiver's chain. Python `_open_inquiries`.
    nonisolated(unsafe) private static var open: [UInt64: [Chain: OpenInquiry]] = [:]
    /// Answers an agent is still OWED, by inquiry pid: added when its inquiry comes
    /// back `accepted`, cleared when a message under that pid arrives. They count
    /// as work in flight, so `await` is honest while one is out. Python `_awaiting`.
    nonisolated(unsafe) private static var owed: [UInt64: [String: Set<String>]] = [:]

    static func register(_ kernel: Kernel, _ agent: String, _ client: String, _ inq: OpenInquiry) {
        lock.withLock { open[kernel.serial, default: [:]][Chain(agent: agent, client: client)] = inq }
    }

    static func isOpen(_ kernel: Kernel, _ agent: String, _ client: String) -> Bool {
        lock.withLock { open[kernel.serial]?[Chain(agent: agent, client: client)] != nil }
    }

    static func take(_ kernel: Kernel, _ agent: String, _ client: String) -> OpenInquiry? {
        lock.withLock {
            open[kernel.serial]?.removeValue(forKey: Chain(agent: agent, client: client))
        }
    }

    static func owe(_ kernel: Kernel, _ agent: String, _ pid: String) {
        lock.withLock { _ = owed[kernel.serial, default: [:]][agent, default: []].insert(pid) }
    }

    static func owedCount(_ kernel: Kernel, _ agent: String) -> Int {
        lock.withLock { owed[kernel.serial]?[agent]?.count ?? 0 }
    }

    /// Settle what `agent` was owed: every `[reply to <pid> · <who> inquiry]` tag in
    /// an incoming message clears that pid. Python `_send`'s `_ANSWER_TAG` scan.
    static func settle(_ kernel: Kernel, _ agent: String, _ text: String) {
        lock.withLock {
            guard var set = owed[kernel.serial]?[agent], !set.isEmpty else { return }
            let ns = text as NSString
            for m in answerTag.matches(in: text, range: NSRange(location: 0, length: ns.length)) {
                set.remove(ns.substring(with: m.range(at: 1)))
            }
            owed[kernel.serial]?[agent] = set
        }
    }

    /// Drop a deleted agent's open inquiries and what it was owed.
    static func forget(_ kernel: Kernel, agent: String) {
        lock.withLock {
            open[kernel.serial] = open[kernel.serial]?.filter { $0.key.agent != agent }
            owed[kernel.serial]?[agent] = nil
        }
    }

    /// Drop a dead kernel's rows — see `AISideTables`.
    static func forget(_ kernel: Kernel) {
        lock.withLock {
            open.removeValue(forKey: kernel.serial)
            owed.removeValue(forKey: kernel.serial)
        }
    }
}

/// Python `_ANSWER_TAG`.
private let answerTag = try! NSRegularExpression(pattern: "\\[reply to (\\S+) \u{b7} \\S+ inquiry\\]")

/// Close the inquiry this chain `(selfId, clientId)` is answering: hand `body`
/// (`{answer}` or `{error}`) to its asker as a message under the inquiry's pid,
/// `[reply to <pid> · <selfId> inquiry]`. The answer is THIS agent's own send to
/// the asker (owner, 2026-10-06), gated like any: it needs `send` on the asker in
/// this agent's `can_call` and the asker's `callable_by` admitting it — a refused
/// delivery is told to this agent (`deliverAnswer`). An operator's inquiry has no
/// conversation to land in: the answer is an event on this agent's inbox instead.
/// No-op when nothing is open. Python `_close_inquiry`.
func closeInquiry(
    kernel: Kernel, selfId: String, clientId: String, _ body: [(String, JSON)]
) async {
    guard let inq = AIInquiries.take(kernel, selfId, clientId) else { return }
    let full = body + [("request_id", inq.requestId)]
    guard let asker = inq.asker else {
        var ev: OrderedDictionary<String, JSON> = ["type": .string("inquiry_answer"), "re": .string(inq.pid)]
        for (k, v) in full { ev[k] = v }
        await kernel.emit(AgentId(selfId), .object(ev))
        return
    }
    var shown: OrderedDictionary<String, JSON> = [:]
    for (k, v) in full where v != .null { shown[k] = v }
    let text = "[reply to \(inq.pid) · \(selfId) inquiry]\n" + renderReply(.object(shown))
    let replyClient = inq.replyClient
    kernel.spawnTask(agent: AgentId(selfId), verb: "inquiry_answer") {
        await deliverAnswer(
            kernel: kernel, selfId: selfId, clientId: clientId, asker: asker,
            replyClient: replyClient, text: text)
    }
}

/// The answer, sent AS THIS AGENT into the asker's conversation. A send to an AI
/// runs the asker's whole turn, so this runs as its own task, never inline. A
/// REFUSED answer is not dropped: this agent is told, in its own chain, that its
/// answer did not reach the asker and why. Python `_deliver_answer`.
private func deliverAnswer(
    kernel: Kernel, selfId: String, clientId: String, asker: String, replyClient: String,
    text: String
) async {
    let out = await kernel.send([
        Item(
            AgentId(asker),
            .object([
                "type": .string("send"), "text": .string(text), "client_id": .string(replyClient),
            ]))
    ])[0]
    let err = out.result["error"]
    if pyTruthy(err) || out.status == .failed || out.status == .timedOut {
        let why = pyTruthy(err) ? pyStr(err) : out.status.rawValue
        deliverSoon(
            kernel: kernel, target: selfId,
            text: "[your inquiry answer did not reach \(asker): \(why)]",
            clientId: clientId, systemPrompt: nil)
    }
}

/// The model's `inquiry_answer`: delivered to the asker of the inquiry this chain
/// is answering. Returns the error block for the model when there is nothing to
/// answer or the answer is the placeholder. Python `_answer_inquiry`.
func answerInquiry(kernel: Kernel, selfId: String, clientId: String, _ call: ToolCall) async
    -> String?
{
    guard AIInquiries.isOpen(kernel, selfId, clientId) else {
        return "inquiry_answer: nothing asked you in this run — an answer goes to an "
            + "inquiry you received (`[inquiry from …]`)"
    }
    let answer = call.arguments["answer"]
    // THE PLACEHOLDER IS NOT AN ANSWER. The shape shown to a model that did not
    // answer is `<answer>what you concluded</answer>`, and a model copies an example
    // back verbatim — measured 2026-09-04 on peer_review, where "..." was written
    // into review.md as the verdict. `answer.strip().strip(".")`: python whitespace
    // FIRST, then dots from both ends.
    guard case .string(let s) = answer, !pyStripDots(pyStrip(s)).isEmpty else {
        return "that is the PLACEHOLDER, not an answer — `answer` came back as "
            + "\(pyRepr(answer)). Say what you actually concluded."
    }
    await closeInquiry(kernel: kernel, selfId: selfId, clientId: clientId, [("answer", .string(s))])
    return nil
}
