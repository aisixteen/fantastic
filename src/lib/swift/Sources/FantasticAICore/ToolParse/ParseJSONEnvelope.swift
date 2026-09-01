// The TAUGHT envelope's body: JSON between one `<tool_call>` tag pair.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`parse_one`).

import FantasticJSON
import Foundation
import OrderedCollections

/// Parse the JSON between one tag pair into `{id, name, arguments}`.
///
/// Lenient (tiny models drift): accepts `{"name","arguments"}`, a `tool` alias
/// for `name`, a flattened object (remaining keys become arguments), and
/// double-encoded (stringified) arguments. Returns nil on unparseable JSON.
func parseOne(_ inner: String) -> ToolCall? {
    // FENCED form first (on the UNTRIMMED block — a trim would eat the
    // program's trailing newline when the close marker was omitted): the JSON
    // part is ONLY what sits above the markers. Parsing the whole block would
    // either choke on the raw program or silently keep the "@body" placeholder
    // as the written contents. Ported from python `tool_parse.py` (fence).
    let nsAll = inner as NSString
    if let (openAt, bodyStart) = fenceOpen(nsAll) {
        let closeAt = fenceCloseLast(nsAll, from: bodyStart) ?? nsAll.length
        let raw = nsAll.substring(with: NSRange(location: bodyStart, length: closeAt - bodyStart))
        guard let call = parseOne(nsAll.substring(to: openAt)) else { return nil }
        var args = call.arguments
        if !plantBody(&args, raw) {
            // No placeholder written — the program still has exactly one home.
            if case .object(var map) = args {
                if case .object(var payload)? = map["payload"] {
                    payload["bytes"] = .string(raw)
                    map["payload"] = .object(payload)
                } else {
                    map["bytes"] = .string(raw)
                }
                args = .object(map)
            }
        }
        return ToolCall(id: call.id, name: call.name, arguments: args)
    }
    let trimmed = inner.trimmingCharacters(in: .whitespacesAndNewlines)
    let bytes = Data(trimmed.utf8)
    // ONE parse, not two. Python tries `json.loads` first and only then
    // `JSONDecoder().raw_decode`; `parsePrefix` IS `raw_decode` and subsumes
    // both, since a body that is nothing BUT the object leaves an empty tail.
    //
    // TRAILING JUNK after a COMPLETE object — observed live from gpt-oss:120b,
    // which closes `arguments` early and then emits `,"reply_to":"self"}}` (one
    // brace too many). Take the object that IS there; the remainder is recovered
    // below rather than thrown away, because the model's real intent is usually
    // in it.
    guard let (value, consumed) = try? JSON.parsePrefix(bytes), let obj = value.asObject
    else {
        // send_binary carrying a RAW program in `bytes` (real newlines, real
        // quotes) is not valid JSON, so the raw lift applies ONLY when the
        // strict parse fails — a model that escaped its whole call correctly
        // parses strictly and gets its escapes DECODED (python E44,
        // 2026-08-31; this was raw-FIRST here, a drift from the canonical
        // order, fixed with the fence port).
        return sendBinaryRaw(trimmed)
    }
    // `consumed` is the only place a BYTE offset appears in this module; the tail
    // becomes text again immediately.
    let rest = String(decoding: bytes.dropFirst(consumed), as: UTF8.self)
        .trimmingCharacters(in: .whitespacesAndNewlines)

    let name = toolName(obj)
    var args: OrderedDictionary<String, JSON>
    switch obj["arguments"] {
    case nil, .some(.null):
        // Absent (python cannot tell an absent key from an explicit null here,
        // and neither branch is a call shape): the object is FLATTENED, so every
        // remaining key is an argument.
        args = strayKeys(obj)
    case .some(.object(let given)):
        // Top-level keys that belong INSIDE arguments. Models routinely put
        // `reply_to` one level too high; `arguments` wins on conflict.
        args = strayKeys(obj)
        if !rest.isEmpty, let tail = recoverStrayTail(rest) {
            for (k, v) in tail { args[k] = v }
        }
        for (k, v) in given { args[k] = v }
    case .some(.string(let encoded)):
        // DOUBLE-ENCODED arguments. Unreadable ⇒ no arguments (python's
        // `except: args = {}`); strays are NOT merged on this path, matching
        // python's `elif`.
        args = (try? JSON.parse(encoded))?.asObject ?? [:]
    default:
        // A number, bool or array in `arguments` is not an arguments object.
        args = [:]
    }

    // THE VERB IN THE TOOL NAME. `send` is the only tool there is, so a model
    // that writes any other name has compressed verb and tool into one — the
    // same compression `parseChannelCall` handles for `call:<agent>.<verb>`,
    // arriving here in the JSON envelope instead. Measured 2026-08-06,
    // gemma-4-12B:
    //   {"name":"reflect","arguments":{"target_id":"clock"}}
    // meaning "reflect the clock". Untranslated, routing read `payload` as
    // absent and dispatched `send([Item("clock", None)])[0].result`, so the handler was called with
    // None and raised `'NoneType' object has no attribute 'get'` — three times in
    // the run this was found in. An explicit `payload` in the args still wins,
    // since the args decide.
    //
    // Both lookups test PRESENCE on the dictionary, deliberately: a `JSON`
    // subscript answers `.null` for a MISSING key, so `args["payload"].isNull`
    // would also translate a call whose payload the model wrote as explicit null.
    if name != "send", args["payload"] == nil, let target = args["target_id"] {
        var inline: OrderedDictionary<String, JSON> = ["type": .string(name)]
        // An explicit `type` among the args overwrites the name-derived one —
        // the args decide — while keeping `type` first, exactly as python's
        // `{"type": name, **args}` does.
        for (k, v) in args { inline[k] = v }
        if let call = inlineSend(target: target, args: inline) { return call }
    }
    return ToolCall(id: mintCallId(), name: name, arguments: .object(args))
}

/// `name`, else the `tool` alias, else `send`.
///
/// Python's `obj.get("name") or obj.get("tool") or "send"` is TRUTHINESS, so an
/// empty or null name falls through to the alias and then to the only tool there
/// is; `str(name)` then stringifies whatever type it was.
private func toolName(_ obj: OrderedDictionary<String, JSON>) -> String {
    for key in ["name", "tool"] {
        guard let v = obj[key], isTruthy(v) else { continue }
        return v.asString ?? v.serialize()
    }
    return "send"
}

/// Python truthiness for a JSON value — `0`, `""`, `[]`, `{}`, `false` and
/// `null` are all falsy.
private func isTruthy(_ v: JSON) -> Bool {
    switch v {
    case .null: return false
    case .bool(let b): return b
    case .integer(let i): return i != 0
    case .double(let d): return d != 0
    case .string(let s): return !s.isEmpty
    case .array(let a): return !a.isEmpty
    case .object(let o): return !o.isEmpty
    }
}

/// The top-level keys that are neither the tool's name nor its arguments slot.
private func strayKeys(_ obj: OrderedDictionary<String, JSON>) -> OrderedDictionary<String, JSON> {
    var out: OrderedDictionary<String, JSON> = [:]
    for (k, v) in obj where k != "name" && k != "tool" && k != "arguments" {
        out[k] = v
    }
    return out
}

/// `,"reply_to":"self"}}` → the k/v pairs that stray tail held.
///
/// `nil` when the tail is not recoverable, in which case it is dropped — a
/// parser that guesses what the model meant is measuring its own guess.
private func recoverStrayTail(_ rest: String) -> OrderedDictionary<String, JSON>? {
    var body = rest
    while body.hasPrefix(",") { body.removeFirst() }
    while body.hasSuffix("}") { body.removeLast() }
    return (try? JSON.parse("{" + body + "}"))?.asObject
}

// ─── the FENCED form: the program rides OUTSIDE the JSON ────────────────────
//
// Measured 2026-09-01 (exercism gym): the model followed the raw-tail contract
// exactly and still lost the call to ONE missing closing brace, because the
// raw tail keeps the payload inside JSON syntax. The fence removes closing
// syntax from the payload path entirely: small flat metadata JSON with
// "@body" where the program goes, the program verbatim between two column-0
// marker lines — never valid code at that position in any language.

/// Column-0 opening marker of the fenced form.
let FENCE_OPEN = "*** Begin Bytes"
/// Column-0 closing marker of the fenced form.
let FENCE_CLOSE = "*** End Bytes"

/// If `marker` at `at` sits alone on its line (column 0, only whitespace
/// after), the index just past that line's newline (or the string's end).
private func markerLineEnd(_ ns: NSString, _ at: Int, _ marker: String) -> Int? {
    if at != 0 && ns.character(at: at - 1) != 0x0A { return nil }
    var i = at + (marker as NSString).length
    while i < ns.length {
        let c = ns.character(at: i)
        if c == 0x0A { return i + 1 }
        if c == 0x20 || c == 0x09 || c == 0x0D { i += 1; continue }
        return nil
    }
    return ns.length
}

/// First line-anchored `FENCE_OPEN`: `(markerStart, bodyStart)`.
private func fenceOpen(_ ns: NSString) -> (Int, Int)? {
    var from = 0
    while from < ns.length {
        let r = ns.range(of: FENCE_OPEN, options: [], range: NSRange(location: from, length: ns.length - from))
        if r.location == NSNotFound { return nil }
        if let bodyStart = markerLineEnd(ns, r.location, FENCE_OPEN) {
            return (r.location, bodyStart)
        }
        from = r.location + r.length
    }
    return nil
}

/// Start of the LAST line-anchored `FENCE_CLOSE` at/after `from` — last, so a
/// program that somehow contains the closer is still taken whole. `nil` means
/// no closer: the caller takes the rest of the block (the envelope's own
/// closing tag already bounds it).
private func fenceCloseLast(_ ns: NSString, from: Int) -> Int? {
    var best: Int? = nil
    var cur = from
    while cur < ns.length {
        let r = ns.range(of: FENCE_CLOSE, options: [], range: NSRange(location: cur, length: ns.length - cur))
        if r.location == NSNotFound { return best }
        if markerLineEnd(ns, r.location, FENCE_CLOSE) != nil { best = r.location }
        cur = r.location + r.length
    }
    return best
}

/// Replace the first `"@body"` placeholder anywhere in the value with the raw
/// program. Returns whether one was found.
private func plantBody(_ node: inout JSON, _ raw: String) -> Bool {
    switch node {
    case .object(var map):
        for (k, v) in map {
            if v.asString == "@body" {
                map[k] = .string(raw)
                node = .object(map)
                return true
            }
            var child = v
            if plantBody(&child, raw) {
                map[k] = child
                node = .object(map)
                return true
            }
        }
        return false
    case .array(var items):
        for i in items.indices {
            var child = items[i]
            if plantBody(&child, raw) {
                items[i] = child
                node = .array(items)
                return true
            }
        }
        return false
    default:
        return false
    }
}

/// The `"bytes":"` opener — the raw-program field of a `send_binary` envelope.
private let sbBytesRegex = try! NSRegularExpression(pattern: "\"bytes\"\\s*:\\s*\"")

/// `send_binary` whose `bytes` field is a program that must NOT be JSON-escaped.
///
/// Ported from python `ai_core/tool_parse.py` `_send_binary_raw`. `json.loads`
/// demands every quote/newline/backslash be escaped, which a model cannot do
/// reliably on a big program — but the PROGRAM is a string we READ, not data we
/// parse. So we lift `bytes` out RAW: everything from `"bytes":"` to the final
/// `"` in the envelope is the program VERBATIM (no unescaping), and only the
/// STRUCTURE around it is JSON-parsed. `bytes` must be the LAST field (nothing
/// but closing braces after its value) so its end is unambiguous — the whole
/// contract. Run FIRST in `parseOne`.
func sendBinaryRaw(_ inner: String) -> ToolCall? {
    guard inner.contains("\"send_binary\"") else { return nil }
    let ns = inner as NSString
    guard
        let m = sbBytesRegex.firstMatch(
            in: inner, range: NSRange(location: 0, length: ns.length))
    else { return nil }
    let valStart = m.range.location + m.range.length  // first char of the raw program
    // bytes is LAST, so its closing quote is the last quote in the envelope.
    let valEnd = ns.range(of: "\"", options: .backwards).location
    if valEnd == NSNotFound || valEnd <= valStart { return nil }
    let raw = ns.substring(with: NSRange(location: valStart, length: valEnd - valStart))
    // Re-close the JSON with an EMPTY bytes value → a well-formed object we can
    // parse for the structure (name, target_id, payload metadata) without
    // touching `raw`.
    let skeleton = ns.substring(to: valStart) + ns.substring(from: valEnd)
    guard let obj = (try? JSON.parse(skeleton))?.asObject,
        obj["name"]?.asString == "send_binary",
        var args = obj["arguments"]?.asObject
    else { return nil }
    // Reinsert the raw string. It lands in `payload` when that is an object
    // (the common shape), else on `arguments` itself — mirroring python.
    if case .object(var payload)? = args["payload"] {
        payload["bytes"] = .string(raw)
        args["payload"] = .object(payload)
    } else {
        args["bytes"] = .string(raw)
    }
    return ToolCall(id: mintCallId(), name: "send_binary", arguments: .object(args))
}
