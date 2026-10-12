// Streaming parts — a model's output split into PARTS as it is written (owner
// design 2026-10-04, `tmp/streaming_parts.md`).
//
// Ported from the canonical Python `ai_core/stream_parts.py` (`PartSplitter`,
// `cut_marker`).
//
// The provider streams raw text; this splitter reads it incrementally, with the
// SAME grammar `ToolParse` reads whole, and yields part lifecycle events:
//
//     .start(part)          a part opened — {stream_id, kind, name?, target?, to?}
//     .delta(part, text)    text of that part as it arrives
//     .end(part, info)      the part closed (`</think>`, `</call>`, or the pass ended on text);
//                           `info` is [:] — for a call, ["ok": bool]: whether the block
//                           parsed (a malformed call is never dispatched)
//     .abort(part, why)     the part never closed: the pass ended, was cut, or was stopped
//
// Kinds: "text" (prose), "thinking" (inside `<think>`), "call" (a `<call
// name="…">` envelope, its args streaming as they are written). A call written
// inside a thought is thinking text, never a call part — only the answer acts
// (owner, 2026-10-07).
//
// It is ANIMATION, never action: nothing here dispatches. The call parser in
// `ToolParse` stays the one authority on what a call IS; this only tells a
// watcher what is being written. A call part names its addressee once
// `<target_id>` (or `<to>`) has closed, so its `start` is held until then — a
// watcher of that addressee sees the whole part.
//
// INDEXING, as in `ToolParse`: one `[Unicode.Scalar]` buffer, so every find,
// prefix and slice is python's CODE-POINT arithmetic (see `Scanning.swift`).

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

private let THINK_OPEN_S: [Unicode.Scalar] = Array(THINK_OPEN.unicodeScalars)
private let THINK_CLOSE_S: [Unicode.Scalar] = Array(THINK_CLOSE.unicodeScalars)
private let NAME_HEAD: [Unicode.Scalar] = Array(#"name=""#.unicodeScalars)
private let QUOTE: [Unicode.Scalar] = ["\""]
private let GT: [Unicode.Scalar] = [">"]
private let ITEM_OPEN: [Unicode.Scalar] = Array("<item>".unicodeScalars)
private let ITEM_CLOSE: [Unicode.Scalar] = Array("</item>".unicodeScalars)

/// One part — python's part dict `{stream_id, kind, name?, target?, to?}`. A
/// REFERENCE, as the dict is: a call part learns its addressee after it opened,
/// and every event and `parts` hold the same object.
final class StreamPart: @unchecked Sendable {
    let streamId: String
    /// "text" | "thinking" | "call"
    let kind: String
    /// The tool a call part names; nil for text and thinking.
    let name: String?
    /// `<target_id>`'s value, once it closed.
    var target: String?
    /// `<to>`'s items, once it closed.
    var to: [String]?

    init(streamId: String, kind: String, name: String? = nil, target: String? = nil,
         to: [String]? = nil) {
        self.streamId = streamId
        self.kind = kind
        self.name = name
        self.target = target
        self.to = to
    }

    /// The part minus its `stream_id`, in python's key order — what a
    /// `part_start` carries.
    var head: OrderedDictionary<String, JSON> {
        var h: OrderedDictionary<String, JSON> = ["kind": .string(kind)]
        if let name { h["name"] = .string(name) }
        if let target { h["target"] = .string(target) }
        if let to { h["to"] = .array(to.map(JSON.string)) }
        return h
    }
}

/// One part lifecycle event — python's `("start"|"delta"|"end"|"abort", part, …)`.
/// `end`'s info is python's dict: empty for text/thinking, `{ok}` for a call.
enum PartEvent {
    case start(StreamPart)
    case delta(StreamPart, String)
    case end(StreamPart, OrderedDictionary<String, JSON>)
    case abort(StreamPart, String)

    var part: StreamPart {
        switch self {
        case .start(let p), .delta(let p, _), .end(let p, _), .abort(let p, _): return p
        }
    }
}

/// How many trailing scalars of `buf` could be the start of one of `tags` — held
/// back until the next chunk says whether they are markup or prose. Python
/// `_held_tail`.
private func heldTail(_ buf: [Unicode.Scalar], _ tags: [[Unicode.Scalar]]) -> Int {
    var best = 0
    for tag in tags {
        var n = min(tag.count - 1, buf.count)
        while n > 0 {
            if tag.starts(with: buf[(buf.count - n)...]) {
                best = max(best, n)
                break
            }
            n -= 1
        }
    }
    return best
}

/// `str.split(sep)` on code points.
private func splitScalars(_ s: [Unicode.Scalar], _ sep: [Unicode.Scalar]) -> [[Unicode.Scalar]] {
    var out: [[Unicode.Scalar]] = []
    var from = 0
    while let at = findScalars(s, sep, from: from) {
        out.append(Array(s[from..<at]))
        from = at + sep.count
    }
    out.append(Array(s[min(from, s.count)...]))
    return out
}

private func joinScalars(_ s: some Collection<Unicode.Scalar>) -> String {
    var view = String.UnicodeScalarView()
    view.append(contentsOf: s)
    return String(view)
}

/// Feed it chunks; it returns part events. One per model pass. Python
/// `PartSplitter`. NOT thread-safe: its owner serializes every call
/// (`AIStreamParts`).
final class PartSplitter {
    private let source: String
    private var buf: [Unicode.Scalar] = []  // unconsumed text
    private(set) var inThink = false
    private var part: StreamPart?  // the open text/thinking part
    private var call: StreamPart?  // the open call part
    private var callBody: [Unicode.Scalar] = []  // the call's body so far (for target detection)
    private var callStarted = false  // its `start` was emitted
    private var callPending: [Unicode.Scalar] = []  // deltas held until the addressee is known
    private var inBytes = false
    /// Every part opened, in order.
    private(set) var parts: [StreamPart] = []
    /// Every CLOSED call part, in order: (stream_id, body) — what a dispatched
    /// call is matched to by its exact body, so the message it becomes carries the
    /// stream it was previewed as (owner, 2026-10-04).
    private(set) var closedCalls: [(streamId: String, body: String)] = []

    init(source: String) {
        self.source = source
    }

    // ── public ──

    func feed(_ chunk: String) -> [PartEvent] {
        buf.append(contentsOf: chunk.unicodeScalars)
        var out: [PartEvent] = []
        while step(&out) {}
        return out
    }

    /// The pass ended: flush prose, END an open text/thinking part, ABORT an open
    /// call (a call that never closed was never a call).
    func finish(_ why: String = "no_close_tag") -> [PartEvent] {
        var out: [PartEvent] = []
        if call != nil {
            callBody += buf
            buf = []
            out += abortCall(why)
        } else {
            if !buf.isEmpty {
                emitText(&out, buf)
                buf = []
            }
            if let p = part {
                if p.kind == "thinking" {
                    out.append(.abort(p, why))
                } else {
                    out.append(.end(p, [:]))
                }
                part = nil
            }
        }
        return out
    }

    /// Stopped mid-stream: every open part ABORTS (nothing is half-ended).
    func stop(_ why: String) -> [PartEvent] {
        var out: [PartEvent] = []
        if call != nil {
            out += abortCall(why)
        }
        if let p = part {
            out.append(.abort(p, why))
            part = nil
        }
        buf = []
        return out
    }

    var openCall: StreamPart? { call }

    // ── internals ──

    private func newPart(_ kind: String, name: String? = nil) -> StreamPart {
        let p = StreamPart(
            streamId: "\(source):" + String(format: "%08x", UInt32.random(in: .min ... .max)),
            kind: kind, name: name)
        parts.append(p)
        return p
    }

    /// Prose or thought, outside any call.
    private func emitText(_ out: inout [PartEvent], _ s: some Collection<Unicode.Scalar>) {
        if s.isEmpty { return }
        let kind = inThink ? "thinking" : "text"
        if part == nil {
            if kind == "text" && s.allSatisfy(pyIsSpace) {
                return  // whitespace between parts is not a part
            }
            part = newPart(kind)
            out.append(.start(part!))
        }
        out.append(.delta(part!, joinScalars(s)))
    }

    private func closeTextPart(_ out: inout [PartEvent]) {
        if let p = part {
            out.append(.end(p, [:]))
            part = nil
        }
    }

    private func step(_ out: inout [PartEvent]) -> Bool {
        if call != nil { return stepCall(&out) }
        // Outside a call: look for the next markup that changes the part. INSIDE
        // thinking only `</think>` does — a `<call>` there is thinking text, never a
        // call part (owner, 2026-10-07: only the answer acts).
        let opens = inThink ? [THINK_CLOSE_S] : [OPEN, THINK_OPEN_S]
        let hits = opens.compactMap { t in findScalars(buf, t, from: 0).map { ($0, t) } }
        guard let hit = hits.min(by: { $0.0 < $1.0 }) else {
            let hold = heldTail(buf, opens)
            emitText(&out, buf[..<(buf.count - hold)])
            buf = Array(buf[(buf.count - hold)...])
            return false
        }
        let (i, tag) = hit
        emitText(&out, buf[..<i])
        if tag == OPEN {
            let rest = Array(buf[(i + OPEN.count)...])
            // `<call` + whitespace + `name="X">` — anything else is prose (`<caller>`).
            if rest.isEmpty {
                buf = Array(buf[i...])
                return false
            }
            if !pyIsSpace(rest[0]) {
                emitText(&out, OPEN)
                buf = rest
                return true
            }
            let head = Array(rest[skipWS(rest, 0)...])
            if head.count < NAME_HEAD.count || !head.starts(with: NAME_HEAD) {
                if NAME_HEAD.starts(with: head) {
                    buf = Array(buf[i...])  // could still become a call
                    return false
                }
                emitText(&out, OPEN)
                buf = rest
                return true
            }
            let q = findScalars(head, QUOTE, from: NAME_HEAD.count)
            let gt = q.flatMap { findScalars(head, GT, from: $0 + 1) }
            guard let q, let gt else {
                buf = Array(buf[i...])
                return false
            }
            closeTextPart(&out)
            call = newPart("call", name: joinScalars(head[NAME_HEAD.count..<q]))
            callBody = []
            callStarted = false
            callPending = []
            inBytes = false
            buf = Array(head[(gt + 1)...])
            return true
        }
        // a thinking boundary
        closeTextPart(&out)
        inThink = tag == THINK_OPEN_S
        buf = Array(buf[(i + tag.count)...])
        return true
    }

    private func stepCall(_ out: inout [PartEvent]) -> Bool {
        let stopTags = inBytes ? [BYTES_CLOSE] : [BYTES_OPEN, CLOSE]
        let hits = stopTags.compactMap { t in findScalars(buf, t, from: 0).map { ($0, t) } }
        guard let hit = hits.min(by: { $0.0 < $1.0 }) else {
            let hold = heldTail(buf, stopTags)
            callText(&out, Array(buf[..<(buf.count - hold)]))
            buf = Array(buf[(buf.count - hold)...])
            return false
        }
        let (i, tag) = hit
        if tag == CLOSE {
            callText(&out, Array(buf[..<i]))
            buf = Array(buf[(i + CLOSE.count)...])
            endCall(&out)
            return true
        }
        callText(&out, Array(buf[..<(i + tag.count)]))
        buf = Array(buf[(i + tag.count)...])
        inBytes = tag == BYTES_OPEN
        return true
    }

    private func callText(_ out: inout [PartEvent], _ s: [Unicode.Scalar]) {
        if s.isEmpty { return }
        callBody += s
        if callStarted {
            out.append(.delta(call!, joinScalars(s)))
            return
        }
        callPending += s
        learnAddressee()
        if addresseeKnown() {
            callStarted = true
            out.append(.start(call!))
            out.append(.delta(call!, joinScalars(callPending)))
            callPending = []
        }
    }

    private func learnAddressee() {
        let body = callBody
        for tag in ["target_id", "to"] {
            let a = findScalars(body, Array("<\(tag)>".unicodeScalars), from: 0)
            let b = a.flatMap { findScalars(body, Array("</\(tag)>".unicodeScalars), from: $0 + 1) }
            guard let a, let b else { continue }
            let lo = a + tag.unicodeScalars.count + 2
            let inner = lo < b ? Array(body[lo..<b]) : []
            if tag == "target_id" {
                call!.target = pyStrip(joinScalars(inner))
            } else {
                let items = splitScalars(inner, ITEM_OPEN).dropFirst().map {
                    pyStrip(joinScalars(splitScalars($0, ITEM_CLOSE)[0]))
                }
                call!.to = items.filter { !$0.isEmpty }
            }
        }
    }

    private func addresseeKnown() -> Bool {
        if call!.target != nil || call!.to != nil { return true }
        // Only `send` names an addressee; every other tool (done, await,
        // inquiry_answer) speaks to the caller from its first byte.
        return call!.name != "send"
    }

    private func endCall(_ out: inout [PartEvent]) {
        if !callStarted {
            callStarted = true
            out.append(.start(call!))
            if !callPending.isEmpty {
                out.append(.delta(call!, joinScalars(callPending)))
            }
        }
        callPending = []
        let body = joinScalars(callBody)
        let ok = parses(" name=\"\(call!.name!)\">" + body)
        closedCalls.append((call!.streamId, body))
        out.append(.end(call!, ["ok": .bool(ok)]))
        call = nil
    }

    private func abortCall(_ why: String) -> [PartEvent] {
        var out: [PartEvent] = []
        if callStarted {
            out.append(.abort(call!, why))
        }
        // An unannounced call (its addressee never written) aborts silently:
        // nobody was shown its start.
        call = nil
        callPending = []
        return out
    }
}

/// The assistant row a CUT pass leaves (owner, 2026-10-04) — a marker on top of
/// the context, never a half answer. `why` is "stopped by <receiver>" (a receiver
/// stopped the stream) or "model stopped generating" (the pass ended with a block
/// still open). Kept: the VISIBLE text streamed so far, thinking stripped; a cut
/// inside an open thought says so; every call that did not go out is NAMED as not
/// sent, never kept as markup. Python `cut_marker`.
func cutMarker(_ textSoFar: String, _ why: String, _ unsent: [StreamPart]) -> String {
    let splitter = PartSplitter(source: "marker")
    let events = splitter.feed(textSoFar)
    let visible = pyStrip(
        events.compactMap { ev -> String? in
            if case .delta(let p, let t) = ev, p.kind == "text" { return t }
            return nil
        }.joined())
    var head = "[\(why)"
    if splitter.inThink && visible.isEmpty {
        head += " while thinking"
    }
    var lines = [head + "]"]
    if !visible.isEmpty {
        lines.append(visible)
    }
    for call in unsent {
        var to = call.target ?? ""
        if to.isEmpty { to = (call.to ?? []).joined(separator: ", ") }
        if to.isEmpty { to = "the caller" }
        lines.append("[a \(call.name ?? "call") call to \(to) was not sent]")
    }
    return lines.joined(separator: "\n")
}
