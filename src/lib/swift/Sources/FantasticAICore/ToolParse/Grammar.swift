// The ONE call grammar: `<call name="…">` with bare child tags, args inline,
// `bytes` verbatim, no JSON anywhere.
//
// A rule-for-rule port of python's `ai_core/tool_parse.py`, which is canonical.
// The e2e harness parses THIS runtime's turns with THAT parser
// (`e2e/helpers/watch.py` imports it whatever the runtime is), so the bar is
// byte compatibility, not merely "a tag parser".
//
//     <call name="send">
//     <target_id>notes</target_id>
//     <reply_to>self</reply_to>
//     <type>patch</type>
//     <edits><item><path>hello.py</path><hunks>2</hunks></item></edits>
//     <bytes>…verbatim: real newlines, real quotes, ends at the first </bytes>…
//     </bytes>
//     </call>
//
// Why tags and not JSON (measured 2026-09-09, qwen3.8-27b, 4 trials × 3 shapes
// that had failed in e2e): JSON with a verbatim `bytes` cut scored 3/12 —
// escaped quotes, literal `\n`, a `\"}` inside content ending the value early;
// bare child tags 12/12, zero escapes. A `"` boundary inside JSON fires the
// model's escaping reflex; a tag boundary carries none.
//
// Nothing is repaired. A call that does not parse is reported with its fault
// named and the minimal shape to send; the model fixes it.
//
// INDEXING, as everywhere in this module: one `[Character]` array and integer
// bounds, so the arithmetic matches python's string indices exactly and the
// step-over-the-OPENER-only rule cannot drift (see `Scanning.swift`).

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

let OPEN: [Character] = Array("<call")
let CLOSE: [Character] = Array("</call>")
let BYTES_OPEN: [Character] = Array("<bytes>")
let BYTES_CLOSE: [Character] = Array("</bytes>")

/// The tools. Any other `name` is refused with the shape — a verb is `<type>`,
/// never the tool name (a compression small models made under the JSON grammar).
let TOOLS = ["send", "done", "await", "inquiry_answer"]

/// Call-level fields of a `send`: where it goes, where the answer goes, what it
/// is for, and the kernel's per-item bounds. Everything else in the call is the
/// payload. The kernel item's caller-chosen execution name is `exec_id` HERE
/// (the router maps it onto the item's `id`): a bare `<id>` would collide with
/// the many verbs whose payload has an `id` (`delete_agent`, `update_agent`, …)
/// now that arguments are inline.
let ROUTING = ["target_id", "to", "re", "reply_to", "title", "timeout", "retries", "exec_id"]

/// The block is a call the model MEANT and it does not parse; `message` is the
/// fault, named for the model.
struct Malformed: Error {
    let message: String
}

// ── scanning ────────────────────────────────────────────────────────────────

/// Index of the `</call>` closing a call whose body starts at `start`, skipping
/// any `</call>` inside a `<bytes>` value; nil if unterminated.
func findClose(_ chars: [Character], _ start: Int) -> Int? {
    var i = start
    while true {
        let c = findChars(chars, CLOSE, from: i)
        let b = findChars(chars, BYTES_OPEN, from: i)
        guard let bAt = b else { return c }
        if let cAt = c, cAt < bAt { return cAt }
        // An open document runs to the end: unterminated.
        guard let e = findChars(chars, BYTES_CLOSE, from: bAt + BYTES_OPEN.count) else {
            return nil
        }
        i = e + BYTES_CLOSE.count
    }
}

/// `(start, bodyStart, end)` for every `<call …>` envelope in document order.
/// `end` is nil when the envelope is unterminated — the LAST thing in the
/// completion, so there is nothing after it to misread.
///
/// `<call` must be followed by whitespace AND a `name`, so `<caller>` is not an
/// envelope: a mention is stepped over by the OPENER ONLY, and never swallows
/// the real call that follows it.
func envelopes(_ chars: [Character]) -> [(start: Int, body: Int, end: Int?)] {
    var out: [(start: Int, body: Int, end: Int?)] = []
    var i = 0
    while i < chars.count {
        guard let pos = findChars(chars, OPEN, from: i) else { return out }
        let after = pos + OPEN.count
        let ws = after < chars.count && chars[after].isWhitespace
        let named = matchesAt(chars, Array("name"), skipWS(chars, after))
        if !ws || !named {
            i = after
            continue
        }
        let end = findClose(chars, after)
        out.append((pos, after, end))
        guard let e = end else { return out }
        i = e + CLOSE.count
    }
    return out
}

/// The next `<name>` open tag at or after `from`, within `[lo, hi)`:
/// `(tagStart, bodyStart, name)`.
///
/// A tag name is a KEY, not an XML name: grant maps are keyed by agent ids and
/// the relative tokens (`$self`, `$parent`, `$children`), and the model writes
/// those as tags (`<$self>*</$self>`, measured 2026-09-09 gated_write, 12
/// refusals) — so `$`, `-` and `.` are letters here. Mirrors python's
/// `<([A-Za-z_$][\w$.\-]*)>`.
func nextTag(_ chars: [Character], from: Int, hi: Int) -> (tag: Int, body: Int, name: String)? {
    var search = from
    while search < hi {
        guard let lt = findChars(chars, ["<"], from: search), lt < hi else { return nil }
        var i = lt + 1
        var name = ""
        if i < hi, chars[i].isASCII, chars[i].isLetter || chars[i] == "_" || chars[i] == "$" {
            name.append(chars[i])
            i += 1
            while i < hi {
                let c = chars[i]
                if c == ">" { return (lt, i + 1, name) }
                if c.isLetter || c.isNumber || c == "_" || c == "$" || c == "." || c == "-" {
                    name.append(c)
                    i += 1
                } else {
                    break
                }
            }
        }
        search = lt + 1
    }
    return nil
}

/// The closer of `<name>` opened just before `start`, honouring nesting of the
/// SAME name and skipping `<bytes>` values inside.
func matchingClose(_ chars: [Character], _ name: String, _ start: Int, hi: Int) -> Int? {
    let opener = Array("<\(name)>")
    let closer = Array("</\(name)>")
    var depth = 1
    var i = start
    while depth > 0 {
        guard let c = findChars(chars, closer, from: i), c < hi else { return nil }
        let o = findChars(chars, opener, from: i).flatMap { $0 < hi ? $0 : nil }
        let b = findChars(chars, BYTES_OPEN, from: i).flatMap { $0 < hi ? $0 : nil }
        let next = [c, o, b].compactMap { $0 }.min() ?? c
        if next == b {
            guard let e = findChars(chars, BYTES_CLOSE, from: next + BYTES_OPEN.count) else {
                return nil
            }
            i = e + BYTES_CLOSE.count
        } else if next == o {
            depth += 1
            i = next + opener.count
        } else {
            depth -= 1
            if depth == 0 { return c }
            i = c + closer.count
        }
    }
    return nil
}

/// One child element of a body: its name and the bounds of its raw value.
struct Element {
    let name: String
    let lo: Int
    let hi: Int
}

/// Every child element of `[lo, hi)`, in order. Text between elements must be
/// whitespace. `bytes` runs verbatim to its FIRST closer.
func elements(_ chars: [Character], lo: Int, hi: Int) throws -> [Element] {
    var out: [Element] = []
    var pos = lo
    var tailLo = lo
    while true {
        guard let tag = nextTag(chars, from: pos, hi: hi) else {
            tailLo = pos
            break
        }
        let between = String(chars[pos..<tag.tag])
        if !pyStrip(between).isEmpty {
            throw Malformed(
                message: "text outside a tag (`\(head40(pyStrip(between)))`) — every "
                    + "argument is its own `<name>…</name>` element")
        }
        let closer = Array("</\(tag.name)>")
        if tag.name == "bytes" {
            guard let end = findChars(chars, closer, from: tag.body), end < hi else {
                throw Malformed(
                    message: "`<bytes>` opened and never closed — close the document with "
                        + "`</bytes>`, then the call with `</call>`")
            }
            out.append(Element(name: tag.name, lo: tag.body, hi: end))
            pos = end + closer.count
            continue
        }
        guard let end = matchingClose(chars, tag.name, tag.body, hi: hi) else {
            throw Malformed(
                message: "`<\(tag.name)>` opened and never closed with `</\(tag.name)>`")
        }
        out.append(Element(name: tag.name, lo: tag.body, hi: end))
        pos = end + closer.count
    }
    let tail = String(chars[tailLo..<hi])
    if !pyStrip(tail).isEmpty {
        throw Malformed(
            message: "text after the last tag (`\(head40(pyStrip(tail)))`) — every argument "
                + "is its own `<name>…</name>` element")
    }
    return out
}

/// The first 40 CHARACTERS of a fault excerpt — python slices `[:40]` on a str,
/// which counts characters.
private func head40(_ s: String) -> String {
    String(s.prefix(40))
}

/// One element's value: verbatim bytes, a nested dict/list, or a typed scalar.
///
/// `bytes` is the one value python delivers as real bytes. `JSON` has no such
/// case, so it rides as the string it was written as — byte-identical, since
/// python's own value is that text encoded utf-8 — and `text_file` takes exactly
/// that (its handler REFUSES a `write`/`patch`/`append` whose `bytes` is not a
/// string).
func value(_ name: String, _ chars: [Character], lo: Int, hi: Int) throws -> JSON {
    if name == "bytes" { return .string(String(chars[lo..<hi])) }
    let raw = String(chars[lo..<hi])
    if nextTag(chars, from: lo, hi: hi) == nil || !pyStrip(raw).hasPrefix("<") {
        return coerceScalar(pyStrip(raw))
    }
    let kids = try elements(chars, lo: lo, hi: hi)
    if kids.isEmpty { return coerceScalar(pyStrip(raw)) }
    // A LIST is a tag whose children are all `<item>` — one element each, any
    // count, one element included. The same tag the reply notation renders, so
    // what the model reads is what it writes (2026-09-09 subai_graph: a reply
    // showed `<allowed><item>…</item></allowed>`, the model wrote it back, and a
    // plural/singular rule read the lone child as a one-field object — three
    // refusals and a JSON-string fallback).
    if kids.allSatisfy({ $0.name == "item" }) {
        return .array(try kids.map { try value($0.name, chars, lo: $0.lo, hi: $0.hi) })
    }
    // `<item>` beside OTHER tags is an ordinary field named item (E14
    // 2026-09-10: `<payload><type>buy</type><item>rocket</item></payload>` was
    // refused as a list error and the purchase never left the model). Only a tag
    // whose children are ALL `<item>` is a list; a one-field object named `item`
    // is the single shape the notation cannot say.
    var out: OrderedDictionary<String, JSON> = [:]
    for kid in kids {
        if out[kid.name] != nil { throw duplicate(kid.name, name) }
        out[kid.name] = try value(kid.name, chars, lo: kid.lo, hi: kid.hi)
    }
    return .object(out)
}

private func duplicate(_ key: String, _ outer: String) -> Malformed {
    Malformed(
        message: "duplicate `<\(key)>` — a list is one tag whose children are all "
            + "`<item>`: `<\(outer)><item>…</item><item>…</item></\(outer)>`")
}

/// The call head: `name="<tool>"` and nothing else before `>`. Returns the tool
/// name and the index just past the `>`. Mirrors python's
/// `^\s*name\s*=\s*"([A-Za-z_][\w]*)"\s*>`.
private func head(_ chars: [Character], lo: Int, hi: Int) -> (name: String, body: Int)? {
    var i = skipWS(chars, lo)
    guard matchesAt(chars, Array("name"), i) else { return nil }
    i = skipWS(chars, i + 4)
    guard i < hi, chars[i] == "=" else { return nil }
    i = skipWS(chars, i + 1)
    guard i < hi, chars[i] == "\"" else { return nil }
    i += 1
    guard i < hi, chars[i].isASCII, chars[i].isLetter || chars[i] == "_" else { return nil }
    var name = ""
    while i < hi, chars[i].isLetter || chars[i].isNumber || chars[i] == "_" {
        name.append(chars[i])
        i += 1
    }
    guard i < hi, chars[i] == "\"" else { return nil }
    let after = skipWS(chars, i + 1)
    guard after < hi, chars[after] == ">" else { return nil }
    return (name, after + 1)
}

/// Parse the text between `<call` and `</call>` into a `ToolCall`.
///
/// Throws for a call-shaped block that does not parse, with the fault named.
/// Returns nil ONLY for a block that is not a call at all (no `name="…">` head)
/// — the model mentioning `<call` in prose.
func parseCall(_ chars: [Character], lo: Int, hi: Int) throws -> ToolCall? {
    guard let h = head(chars, lo: lo, hi: hi) else {
        let upTo = findChars(chars, [">"], from: lo).map { min($0, hi) } ?? hi
        let headText = pyStrip(String(chars[lo..<upTo]))
        if !headText.hasPrefix("name=") { return nil }
        throw Malformed(
            message: "the call head must be exactly `name=\"<tool>\"` — nothing else before `>`")
    }
    guard TOOLS.contains(h.name) else {
        throw Malformed(
            message: "`\(h.name)` is not a tool — the tools are \(TOOLS.joined(separator: ", ")). "
                + "A VERB goes in `<type>`: `<call name=\"send\"><target_id>…</target_id>"
                + "<type>\(h.name)</type></call>`")
    }
    var fields: OrderedDictionary<String, JSON> = [:]
    for el in try elements(chars, lo: h.body, hi: hi) {
        if fields[el.name] != nil { throw duplicate(el.name, el.name) }
        fields[el.name] = try value(el.name, chars, lo: el.lo, hi: el.hi)
    }
    if h.name != "send" {
        return ToolCall(id: mintCallId(), name: h.name, arguments: .object(fields))
    }
    let rawTarget = fields.removeValue(forKey: "target_id")
    let to = normalizeTo(fields.removeValue(forKey: "to"))
    let target = rawTarget.flatMap(\.asString).map(pyStrip).flatMap { $0.isEmpty ? nil : $0 }
    if target == nil, to == nil {
        throw Malformed(
            message: "a send names its target: `<target_id>agent id</target_id>`, or several "
                + "recipients as `<to><item>id</item><item>id</item></to>`")
    }
    var args: OrderedDictionary<String, JSON> = [:]
    if let t = target { args["target_id"] = .string(t) }
    if let list = to { args["to"] = .array(list) }
    for key in ROUTING.dropFirst(2) {
        if let v = fields.removeValue(forKey: key) { args[key] = v }
    }
    args["payload"] = .object(fields)
    return ToolCall(id: mintCallId(), name: "send", arguments: .object(args))
}

/// `to` as python leaves it: a non-blank string becomes a one-element list, a
/// list keeps its non-blank members as trimmed strings, anything else (a bare
/// number, an object) is not a recipient list and falls through to the "names
/// its target" refusal.
private func normalizeTo(_ raw: JSON?) -> [JSON]? {
    switch raw {
    case .some(.string(let s)):
        let t = pyStrip(s)
        return t.isEmpty ? nil : [.string(t)]
    case .some(.array(let items)):
        let kept = items.map(scalarText).filter { !$0.isEmpty }.map { JSON.string($0) }
        return kept.isEmpty ? nil : kept
    default:
        return nil
    }
}

/// One recipient as text — python's `str(t).strip()` over whatever the notation
/// produced.
private func scalarText(_ v: JSON) -> String {
    switch v {
    case .string(let s): return pyStrip(s)
    case .null: return ""
    default: return v.serialize()
    }
}
