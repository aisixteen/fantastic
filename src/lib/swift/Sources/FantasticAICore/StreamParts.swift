// Streaming parts — a model's output split into PARTS as it is written.
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
// inside a thought is thinking text, never a call part — only the answer acts.
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
private func heldTail(_ buf: ArraySlice<Unicode.Scalar>, _ tags: [[Unicode.Scalar]]) -> Int {
    var best = 0
    for tag in tags {
        var n = min(tag.count - 1, buf.count)
        while n > 0 {
            if tag.starts(with: buf[(buf.endIndex - n)...]) {
                best = max(best, n)
                break
            }
            n -= 1
        }
    }
    return best
}

/// The EARLIEST of `tags` at or after `from` — one pass over the text, every
/// tag tried at each position (in list order, so a tie keeps the first tag as
/// `min` over per-tag finds did). Per-tag finds each walked to the END when
/// their tag was absent, which re-read the whole unconsumed text every step.
private func firstOf(
    _ hay: [Unicode.Scalar], _ tags: [[Unicode.Scalar]], from: Int
) -> (Int, [Unicode.Scalar])? {
    var at = max(0, from)
    let n = hay.count
    while at < n {
        let c = hay[at]
        for t in tags where t[0] == c && matchesAt(hay, t, at) { return (at, t) }
        at += 1
    }
    return nil
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
    private var buf: [Unicode.Scalar] = []  // text fed; `buf[pos...]` is unconsumed
    private var pos = 0  // where the unconsumed text starts — consuming never copies
    private(set) var inThink = false
    private var part: StreamPart?  // the open text/thinking part
    private var call: StreamPart?  // the open call part
    private var callBody: [Unicode.Scalar] = []  // the call's body so far (for target detection)
    private var callStarted = false  // its `start` was emitted
    private var callPending: [Unicode.Scalar] = []  // deltas held until the addressee is known
    private var inBytes = false
    /// `learnAddressee`'s scan state per tag, so a growing body is never re-read
    /// from 0: where the open tag sits once found, and where each search resumes.
    private var addrOpenAt: [String: Int] = [:]
    private var addrOpenFrom: [String: Int] = [:]
    private var addrCloseFrom: [String: Int] = [:]
    /// Every part opened, in order.
    private(set) var parts: [StreamPart] = []
    /// Every CLOSED call part, in order: (stream_id, body) — what a dispatched
    /// call is matched to by its exact body, so the message it becomes carries the
    /// stream it was previewed as.
    private(set) var closedCalls: [(streamId: String, body: String)] = []
    /// STOPPED is terminal: the stop runs while the pass's own task is still alive,
    /// so a chunk the provider already handed over may still be fed — it opens no
    /// part after the aborts (python `PartSplitter._stopped`).
    private var stopped = false

    init(source: String) {
        self.source = source
    }

    // ── public ──

    func feed(_ chunk: String) -> [PartEvent] {
        if stopped { return [] }
        buf.append(contentsOf: chunk.unicodeScalars)
        var out: [PartEvent] = []
        while step(&out) {}
        // Drop the consumed head (what is left is a held tail or a pending head).
        if pos > 0 {
            buf.removeFirst(pos)
            pos = 0
        }
        return out
    }

    /// The pass ended: flush prose, END an open text/thinking part, ABORT an open
    /// call (a call that never closed was never a call).
    func finish(_ why: String = "no_close_tag") -> [PartEvent] {
        if stopped { return [] }
        var out: [PartEvent] = []
        if call != nil {
            callBody += buf[pos...]
            buf = []
            pos = 0
            out += abortCall(why)
        } else {
            if pos < buf.count {
                emitText(&out, buf[pos...])
            }
            buf = []
            pos = 0
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

    /// Stopped mid-stream: every open part ABORTS (nothing is half-ended), and
    /// nothing fed after opens another.
    func stop(_ why: String) -> [PartEvent] {
        stopped = true
        var out: [PartEvent] = []
        if call != nil {
            out += abortCall(why)
        }
        if let p = part {
            out.append(.abort(p, why))
            part = nil
        }
        buf = []
        pos = 0
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
        // call part (only the answer acts).
        let opens = inThink ? [THINK_CLOSE_S] : [OPEN, THINK_OPEN_S]
        guard let (i, tag) = firstOf(buf, opens, from: pos) else {
            let hold = heldTail(buf[pos...], opens)
            emitText(&out, buf[pos..<(buf.count - hold)])
            pos = buf.count - hold
            return false
        }
        emitText(&out, buf[pos..<i])
        if tag == OPEN {
            let restAt = i + OPEN.count
            // `<call` + whitespace + `name="X">` — anything else is prose (`<caller>`).
            if restAt >= buf.count {
                pos = i
                return false
            }
            if !pyIsSpace(buf[restAt]) {
                emitText(&out, OPEN)
                pos = restAt
                return true
            }
            let headAt = skipWS(buf, restAt)
            let head = buf[headAt...]
            if head.count < NAME_HEAD.count || !head.starts(with: NAME_HEAD) {
                if NAME_HEAD.starts(with: head) {
                    pos = i  // could still become a call
                    return false
                }
                emitText(&out, OPEN)
                pos = restAt
                return true
            }
            let q = findScalars(buf, QUOTE, from: headAt + NAME_HEAD.count)
            let gt = q.flatMap { findScalars(buf, GT, from: $0 + 1) }
            guard let q, let gt else {
                pos = i
                return false
            }
            closeTextPart(&out)
            call = newPart("call", name: joinScalars(buf[(headAt + NAME_HEAD.count)..<q]))
            callBody = []
            callStarted = false
            callPending = []
            inBytes = false
            addrOpenAt = [:]
            addrOpenFrom = [:]
            addrCloseFrom = [:]
            pos = gt + 1
            return true
        }
        // a thinking boundary
        closeTextPart(&out)
        inThink = tag == THINK_OPEN_S
        pos = i + tag.count
        return true
    }

    private func stepCall(_ out: inout [PartEvent]) -> Bool {
        let stopTags = inBytes ? [BYTES_CLOSE] : [BYTES_OPEN, CLOSE]
        guard let (i, tag) = firstOf(buf, stopTags, from: pos) else {
            let hold = heldTail(buf[pos...], stopTags)
            callText(&out, buf[pos..<(buf.count - hold)])
            pos = buf.count - hold
            return false
        }
        if tag == CLOSE {
            callText(&out, buf[pos..<i])
            pos = i + CLOSE.count
            endCall(&out)
            return true
        }
        callText(&out, buf[pos..<(i + tag.count)])
        pos = i + tag.count
        inBytes = tag == BYTES_OPEN
        return true
    }

    private func callText(_ out: inout [PartEvent], _ s: ArraySlice<Unicode.Scalar>) {
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
            // The FIRST `<tag>` and the first `</tag>` after it, as a from-0
            // search finds them — resumed where the last search stopped (a hit
            // never moves once the body only grows).
            let open = Array("<\(tag)>".unicodeScalars)
            let close = Array("</\(tag)>".unicodeScalars)
            if addrOpenAt[tag] == nil {
                guard let a = findScalars(body, open, from: addrOpenFrom[tag] ?? 0) else {
                    addrOpenFrom[tag] = max(0, body.count - open.count + 1)
                    continue
                }
                addrOpenAt[tag] = a
            }
            let a = addrOpenAt[tag]!
            guard let b = findScalars(body, close, from: max(a + 1, addrCloseFrom[tag] ?? 0)) else {
                addrCloseFrom[tag] = max(a + 1, body.count - close.count + 1)
                continue
            }
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

/// The assistant row a CUT pass leaves — a marker on top of
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
