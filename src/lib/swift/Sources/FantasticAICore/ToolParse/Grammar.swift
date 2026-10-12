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
// INDEXING, as everywhere in this module: one `[Unicode.Scalar]` array and
// integer bounds, so the arithmetic matches python's CODE-POINT string indices
// exactly and the step-over-the-OPENER-only rule cannot drift. A `[Character]`
// array does not match it — see `Scanning.swift`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

let OPEN: [Unicode.Scalar] = Array("<call".unicodeScalars)
let CLOSE: [Unicode.Scalar] = Array("</call>".unicodeScalars)
let BYTES_OPEN: [Unicode.Scalar] = Array("<bytes>".unicodeScalars)
let BYTES_CLOSE: [Unicode.Scalar] = Array("</bytes>".unicodeScalars)

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
func findClose(_ chars: [Unicode.Scalar], _ start: Int) -> Int? {
    var i = start
    while true {
        let c = findScalars(chars, CLOSE, from: i)
        let b = findScalars(chars, BYTES_OPEN, from: i)
        guard let bAt = b else { return c }
        if let cAt = c, cAt < bAt { return cAt }
        // An open document runs to the end: unterminated.
        guard let e = findScalars(chars, BYTES_CLOSE, from: bAt + BYTES_OPEN.count) else {
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
func envelopes(_ chars: [Unicode.Scalar]) -> [(start: Int, body: Int, end: Int?)] {
    var out: [(start: Int, body: Int, end: Int?)] = []
    var i = 0
    while i < chars.count {
        guard let pos = findScalars(chars, OPEN, from: i) else { return out }
        let after = pos + OPEN.count
        let ws = after < chars.count && pyIsSpace(chars[after])
        let named = matchesAt(chars, Array("name".unicodeScalars), skipWS(chars, after))
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
/// the relative tokens (`$self`, `$parent`, `$subtree`), and the model writes
/// those as tags (`<$self>*</$self>`, measured 2026-09-09 gated_write, 12
/// refusals) — so `$`, `-` and `.` are letters here. Mirrors python's
/// `<([A-Za-z_$][\w$.\-]*)>`.
func nextTag(_ chars: [Unicode.Scalar], from: Int, hi: Int) -> (
    tag: Int, body: Int, name: [Unicode.Scalar]
)? {
    var search = from
    while search < hi {
        guard let lt = findScalars(chars, ["<"], from: search), lt < hi else { return nil }
        var i = lt + 1
        var name: [Unicode.Scalar] = []
        if i < hi, isNameStart(chars[i]) {
            name.append(chars[i])
            i += 1
            while i < hi {
                let c = chars[i]
                if c == ">" { return (lt, i + 1, name) }
                if isNameContinue(c) {
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

/// `[A-Za-z_$]` — ASCII, exactly as python's first character class is.
private func isNameStart(_ c: Unicode.Scalar) -> Bool {
    switch c {
    case "A"..."Z", "a"..."z", "_", "$": return true
    default: return false
    }
}

/// `[\w$.\-]` — python's `\w` is `_` plus `Py_UNICODE_ISALNUM`: general
/// categories Lu/Ll/Lt/Lm/Lo, or a scalar with a NUMERIC TYPE (Nd/Nl/No). Swift's
/// `isLetter`/`isNumber` on a Character admit Other_Alphabetic and symbols
/// python's class does not, so `<k_नाम>` and `<xⒶ>` dispatched here where python
/// reads them as text outside a tag.
private func isNameContinue(_ c: Unicode.Scalar) -> Bool {
    if c == "_" || c == "$" || c == "." || c == "-" { return true }
    if c.properties.numericType != nil { return true }
    switch c.properties.generalCategory {
    case .uppercaseLetter, .lowercaseLetter, .titlecaseLetter, .modifierLetter, .otherLetter:
        return true
    default:
        return false
    }
}

/// The closer of `<name>` opened just before `start`, honouring nesting of the
/// SAME name and skipping `<bytes>` values inside.
func matchingClose(_ chars: [Unicode.Scalar], _ name: [Unicode.Scalar], _ start: Int, hi: Int)
    -> Int?
{
    let opener = ["<"] + name + [">"]
    let closer = ["<", "/"] + name + [">"]
    var depth = 1
    var i = start
    while depth > 0 {
        guard let c = findScalars(chars, closer, from: i), c < hi else { return nil }
        let o = findScalars(chars, opener, from: i).flatMap { $0 < hi ? $0 : nil }
        let b = findScalars(chars, BYTES_OPEN, from: i).flatMap { $0 < hi ? $0 : nil }
        let next = [c, o, b].compactMap { $0 }.min() ?? c
        if next == b {
            guard let e = findScalars(chars, BYTES_CLOSE, from: next + BYTES_OPEN.count) else {
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
    /// The tag name as CODE POINTS. Two names are the same name only when their
    /// scalars are — a `String` key compares canonically equivalent forms EQUAL,
    /// which would call an NFC/NFD pair a duplicate where python has two fields.
    let name: [Unicode.Scalar]
    let lo: Int
    let hi: Int

    var text: String { String(String.UnicodeScalarView(name)) }
}

/// Every child element of `[lo, hi)`, in order. Text between elements must be
/// whitespace. `bytes` runs verbatim to its FIRST closer.
func elements(_ chars: [Unicode.Scalar], lo: Int, hi: Int) throws -> [Element] {
    var out: [Element] = []
    var pos = lo
    var tailLo = lo
    while true {
        guard let tag = nextTag(chars, from: pos, hi: hi) else {
            tailLo = pos
            break
        }
        let between = scalarText(chars, pos, tag.tag)
        if !pyStrip(between).isEmpty {
            throw Malformed(
                message: "text outside a tag (`\(head40(pyStrip(between)))`) — every "
                    + "argument is its own `<name>…</name>` element")
        }
        let closer: [Unicode.Scalar] = ["<", "/"] + tag.name + [">"]
        if tag.name == BYTES_NAME {
            guard let end = findScalars(chars, closer, from: tag.body), end < hi else {
                throw Malformed(
                    message: "`<bytes>` opened and never closed — close the document with "
                        + "`</bytes>`, then the call with `</call>`")
            }
            out.append(Element(name: tag.name, lo: tag.body, hi: end))
            pos = end + closer.count
            continue
        }
        guard let end = matchingClose(chars, tag.name, tag.body, hi: hi) else {
            let n = String(String.UnicodeScalarView(tag.name))
            throw Malformed(message: "`<\(n)>` opened and never closed with `</\(n)>`")
        }
        out.append(Element(name: tag.name, lo: tag.body, hi: end))
        pos = end + closer.count
    }
    let tail = scalarText(chars, tailLo, hi)
    if !pyStrip(tail).isEmpty {
        throw Malformed(
            message: "text after the last tag (`\(head40(pyStrip(tail)))`) — every argument "
                + "is its own `<name>…</name>` element")
    }
    return out
}

/// The first 40 CODE POINTS of a fault excerpt — python slices `[:40]` on a str,
/// which counts code points, and a CRLF excerpt of 13 `ab\r\n` groups is 13
/// graphemes but 40 scalars.
private func head40(_ s: String) -> String {
    String(String.UnicodeScalarView(s.unicodeScalars.prefix(40)))
}

/// `bytes`, as scalars — the one element name the scanner treats as verbatim.
private let BYTES_NAME: [Unicode.Scalar] = Array("bytes".unicodeScalars)

/// `[A-Za-z_]` — the tool-name head class of python's `_HEAD`.
private func isToolNameStart(_ c: Unicode.Scalar) -> Bool {
    switch c {
    case "A"..."Z", "a"..."z", "_": return true
    default: return false
    }
}

/// python's `\w`, without the tag-name extras: `_`, a letter category, or a
/// scalar with a numeric type.
private func isWordScalar(_ c: Unicode.Scalar) -> Bool {
    if c == "_" { return true }
    if c.properties.numericType != nil { return true }
    switch c.properties.generalCategory {
    case .uppercaseLetter, .lowercaseLetter, .titlecaseLetter, .modifierLetter, .otherLetter:
        return true
    default:
        return false
    }
}

/// One element's value: verbatim bytes, a nested dict/list, or a typed scalar.
///
/// `bytes` is the one value python delivers as real bytes. `JSON` has no such
/// case, so it rides as the string it was written as — byte-identical, since
/// python's own value is that text encoded utf-8 — and file_bridge's text verbs
/// take exactly that (`write`/`edit`/`append` REFUSE a `bytes` that is not a
/// string).
func value(_ name: [Unicode.Scalar], _ chars: [Unicode.Scalar], lo: Int, hi: Int) throws -> JSON {
    if name == BYTES_NAME { return .string(scalarText(chars, lo, hi)) }
    let raw = scalarText(chars, lo, hi)
    // `raw.strip().startswith("<")` on a CODE POINT: `hasPrefix` compares
    // Characters, so `<` followed by a combining mark is one grapheme that is not
    // `<` — python sees the `<` and descends.
    if nextTag(chars, from: lo, hi: hi) == nil
        || pyStrip(raw).unicodeScalars.first != "<"
    {
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
    if kids.allSatisfy({ $0.name == ITEM_NAME }) {
        return .array(try kids.map { try value($0.name, chars, lo: $0.lo, hi: $0.hi) })
    }
    // `<item>` beside OTHER tags is an ordinary field named item (E14
    // 2026-09-10: `<payload><type>buy</type><item>rocket</item></payload>` was
    // refused as a list error and the purchase never left the model). Only a tag
    // whose children are ALL `<item>` is a list; a one-field object named `item`
    // is the single shape the notation cannot say.
    var out: OrderedDictionary<String, JSON> = [:]
    var seen: [[Unicode.Scalar]] = []
    for kid in kids {
        if seen.contains(kid.name) {
            throw duplicate(kid.text, String(String.UnicodeScalarView(name)))
        }
        seen.append(kid.name)
        out[kid.text] = try value(kid.name, chars, lo: kid.lo, hi: kid.hi)
    }
    return .object(out)
}

/// `item`, as scalars — a list is a tag whose children are ALL named this.
private let ITEM_NAME: [Unicode.Scalar] = Array("item".unicodeScalars)

private func duplicate(_ key: String, _ outer: String) -> Malformed {
    Malformed(
        message: "duplicate `<\(key)>` — a list is one tag whose children are all "
            + "`<item>`: `<\(outer)><item>…</item><item>…</item></\(outer)>`")
}

/// The call head: `name="<tool>"` and nothing else before `>`. Returns the tool
/// name and the index just past the `>`. Mirrors python's
/// `^\s*name\s*=\s*"([A-Za-z_][\w]*)"\s*>`.
private func head(_ chars: [Unicode.Scalar], lo: Int, hi: Int) -> (name: String, body: Int)? {
    var i = skipWS(chars, lo)
    guard matchesAt(chars, Array("name".unicodeScalars), i) else { return nil }
    i = skipWS(chars, i + 4)
    guard i < hi, chars[i] == "=" else { return nil }
    i = skipWS(chars, i + 1)
    guard i < hi, chars[i] == "\"" else { return nil }
    i += 1
    guard i < hi, isToolNameStart(chars[i]) else { return nil }
    var name = String.UnicodeScalarView()
    while i < hi, isWordScalar(chars[i]) {
        name.append(chars[i])
        i += 1
    }
    guard i < hi, chars[i] == "\"" else { return nil }
    let after = skipWS(chars, i + 1)
    guard after < hi, chars[after] == ">" else { return nil }
    return (String(name), after + 1)
}

/// Parse the text between `<call` and `</call>` into a `ToolCall`.
///
/// Throws for a call-shaped block that does not parse, with the fault named.
/// Returns nil ONLY for a block that is not a call at all (no `name="…">` head)
/// — the model mentioning `<call` in prose.
func parseCall(_ chars: [Unicode.Scalar], lo: Int, hi: Int) throws -> ToolCall? {
    guard let h = head(chars, lo: lo, hi: hi) else {
        let upTo = findScalars(chars, [">"], from: lo).map { min($0, hi) } ?? hi
        let headText = pyStrip(scalarText(chars, lo, upTo))
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
    var seen: [[Unicode.Scalar]] = []
    for el in try elements(chars, lo: h.body, hi: hi) {
        if seen.contains(el.name) { throw duplicate(el.text, el.text) }
        seen.append(el.name)
        fields[el.text] = try value(el.name, chars, lo: el.lo, hi: el.hi)
    }
    if h.name != "send" {
        return ToolCall(id: mintCallId(), name: h.name, arguments: .object(fields))
    }
    let rawTarget = fields.removeValue(forKey: "target_id")
    let to = normalizeTo(fields.removeValue(forKey: "to"))
    // A NUMERIC-SHAPED target_id is not a string target — python's
    // `coerce_scalar` types it as an (unbounded) `int`, so `isinstance(target,
    // str)` fails even for a value too wide for this carrier's `Int64`, which
    // `coerceScalar` otherwise has to keep as `.string` text (C74, D13). Check
    // the raw text's shape, not just the JSON case, so a huge id still refuses
    // like python instead of dispatching to a literal numeral agent id.
    let target = rawTarget.flatMap(\.asString).map(pyStrip).flatMap {
        $0.isEmpty || coerceScalarIsIntegerShaped($0) ? nil : $0
    }
    // REFUSED ONLY WHEN THERE IS NO STRING TARGET **AND** `to` IS PYTHON-FALSY.
    // A truthy non-list `to` (`<to>42</to>`) is a CALL python dispatches — the
    // no-target reply comes from routing, not from the parser.
    if target == nil, !(to.map(pyTruthy) ?? false) {
        throw Malformed(
            message: "a send names its target: `<target_id>agent id</target_id>`, or several "
                + "recipients as `<to><item>id</item><item>id</item></to>`")
    }
    var args: OrderedDictionary<String, JSON> = [:]
    if let t = target { args["target_id"] = .string(t) }
    if let t = to, pyTruthy(t) { args["to"] = t }
    for key in ROUTING.dropFirst(2) {
        if let v = fields.removeValue(forKey: key) { args[key] = v }
    }
    args["payload"] = .object(fields)
    return ToolCall(id: mintCallId(), name: "send", arguments: .object(args))
}

/// `to` as python leaves it, and python leaves MOST of it alone: a non-blank
/// string becomes a one-element list, a list keeps the non-blank `str(t).strip()`
/// of each member, and ANY other value is kept exactly as written.
///
/// A bare number, a bool or an object is therefore still a call — `<to>42</to>`
/// parses and routing answers the no-target reply — where mapping it to "no
/// recipients" refused at parse time and the model was told its call was
/// malformed instead of being told what was missing.
private func normalizeTo(_ raw: JSON?) -> JSON? {
    switch raw {
    case .some(.string(let s)):
        let t = pyStrip(s)
        // A blank string is NOT converted: python's `if … and to.strip()` leaves
        // it the falsy string it was.
        return t.isEmpty ? raw : .array([.string(t)])
    case .some(.array(let items)):
        return .array(
            items.map { pyStrip(pyStr($0)) }.filter { !$0.isEmpty }.map { JSON.string($0) })
    default:
        return raw
    }
}
