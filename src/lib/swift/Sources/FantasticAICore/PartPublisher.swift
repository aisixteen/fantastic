// Where a streaming part's events go — python `ai_core/core.py` `_PartPublisher`
// — and the pair the in-flight entry carries (python's `(splitter, parts_out)`).

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Routes part events (`StreamParts`) to their addressees, deltas BATCHED so a
/// token-rate stream does not become a token-rate event stream.
///
/// A call part with an addressee goes to that inbox (each of `to`), as an emit —
/// gated like any emit on another agent; a refused one is dropped. Prose, thought
/// and calls without an addressee (done, await, inquiry_answer) go where the
/// tokens go: the caller (`toCaller`).
final class PartPublisher {
    static let BATCH_SECONDS = 0.05

    let kernel: Kernel
    let selfId: AgentId
    let clientId: String
    /// THE ANCHOR: the message this turn answers — where a consumer places the
    /// draft in its own (possibly folded) history. Python `re=current_task()`.
    let re: Int64?
    private var pending: OrderedDictionary<String, (StreamPart, String)> = [:]
    private var last: [String: Double] = [:]

    init(kernel: Kernel, selfId: AgentId, clientId: String, re: Int64? = nil) {
        self.kernel = kernel
        self.selfId = selfId
        self.clientId = clientId
        self.re = re
    }

    /// python `time.monotonic()`.
    private func now() -> Double { ProcessInfo.processInfo.systemUptime }

    private func send(_ part: StreamPart, _ event: OrderedDictionary<String, JSON>) async {
        var ev = event
        ev["stream_id"] = .string(part.streamId)
        ev["source"] = .string(selfId.value)
        // `part.get("to") or ([part["target"]] if part.get("target") else [])`
        var addrs: [String] = part.to ?? []
        if addrs.isEmpty, let t = part.target, !t.isEmpty { addrs = [t] }
        if part.kind != "call" || addrs.isEmpty {
            await toCaller(kernel: kernel, agentId: selfId, clientId: clientId, event: ev)
            return
        }
        for addr in addrs {
            await kernel.emit(addr == "self" ? selfId : AgentId(addr), .object(ev))
        }
    }

    private func flush(_ sid: String) async {
        guard let held = pending.removeValue(forKey: sid) else { return }
        let (part, text) = held
        last[sid] = now()
        await send(part, ["type": .string("part_delta"), "text": .string(text)])
    }

    func push(_ events: [PartEvent], final: Bool = false) async {
        for ev in events {
            let part = ev.part
            let sid = part.streamId
            switch ev {
            case .start:
                var e: OrderedDictionary<String, JSON> = ["type": .string("part_start")]
                for (k, v) in part.head { e[k] = v }
                if let re { e["re"] = .integer(re) }
                await send(part, e)
                last[sid] = now()
            case .delta(_, let text):
                let prev = pending[sid]?.1 ?? ""
                pending[sid] = (part, prev + text)
                if now() - (last[sid] ?? 0) >= Self.BATCH_SECONDS {
                    await flush(sid)
                }
            case .end(_, let info):
                await flush(sid)
                var e: OrderedDictionary<String, JSON> = ["type": .string("part_end")]
                for (k, v) in info { e[k] = v }
                await send(part, e)
            case .abort(_, let why):
                pending.removeValue(forKey: sid)
                await send(part, ["type": .string("part_abort"), "reason": .string(why)])
            }
        }
        if final {
            for sid in Array(pending.keys) {
                await flush(sid)
            }
        }
    }
}

/// The splitter and the publisher of ONE pass, as the in-flight entry carries
/// them — python's `(splitter, parts_out)` tuple on `_current[id]["parts"]`.
///
/// SWIFT-SPECIFIC: the stream loop and `onCancel` reach this pair from two
/// tasks that run in PARALLEL, where python's two coroutines interleave only at
/// an `await`. Each split-and-publish runs whole under `gate`, so a part's events
/// keep the splitter's order, and once a stop has aborted the open parts the
/// loop — still alive until the kernel's abort lands — opens no new one.
final class AIStreamParts: @unchecked Sendable {
    let splitter: PartSplitter
    let publisher: PartPublisher
    private let gate = AIFifoLock()
    private var stopped = false

    init(splitter: PartSplitter, publisher: PartPublisher) {
        self.splitter = splitter
        self.publisher = publisher
    }

    /// `parts_out.push(splitter.feed(chunk))`.
    func feed(_ chunk: String) async {
        await gate.acquire()
        if !stopped {
            await publisher.push(splitter.feed(chunk))
        }
        await gate.release()
    }

    /// `cut = splitter.open_call; parts_out.push(splitter.finish(), final=True)` —
    /// the call still open when the model stopped generating, if any.
    func finish() async -> StreamPart? {
        await gate.acquire()
        let cut = splitter.openCall
        await publisher.push(splitter.finish(), final: true)
        await gate.release()
        return cut
    }

    /// `parts_out.push(splitter.stop(why), final=True)`. Returns every part the
    /// pass opened (`splitter.parts`), read in the same step.
    @discardableResult
    func stop(_ why: String) async -> [StreamPart] {
        await gate.acquire()
        stopped = true
        let opened = splitter.parts
        await publisher.push(splitter.stop(why), final: true)
        await gate.release()
        return opened
    }

    /// Every part this pass opened, in order (`splitter.parts`).
    func parts() async -> [StreamPart] {
        await gate.acquire()
        let out = splitter.parts
        await gate.release()
        return out
    }

    /// Every closed call part, in order (`splitter.closed_calls`): (stream_id, body).
    func closedCalls() async -> [(streamId: String, body: String)] {
        await gate.acquire()
        let out = splitter.closedCalls
        await gate.release()
        return out
    }
}
