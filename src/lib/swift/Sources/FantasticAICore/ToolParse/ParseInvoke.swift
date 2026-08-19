// Claude's own envelope: `<invoke name="…"><parameter name="…">…</parameter></invoke>`.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`_INVOKE_NAME`,
// `_INVOKE_PARAM`, `parse_invoke`). Python matches the tag with two regexes;
// this reads the same two shapes with the hand-written scanners in
// `Scanning.swift`, because the port carries no regex dependency.
//
// The exposure is the anthropic backends: refusing this wrapper dropped whole
// runs at pass 1 on claude-haiku-4.5, and the model — never receiving a reply —
// then invented the entire remaining session. See `INVOKE_OPEN` in
// `Envelopes.swift` for the measurement.

import FantasticJSON
import Foundation
import OrderedCollections

/// Parse one `<invoke name="…">…</invoke>` body into `{id, name, arguments}`.
func parseInvoke(_ block: String) -> ToolCall? {
    let chars = Array(block)
    // Python's `block.partition(">")`: the HEAD is the opening tag's own
    // attributes (the block starts just after `<invoke`), the BODY is everything
    // after the first `>`. No `>` at all leaves an empty body, which then holds no
    // parameters and is not a call.
    let cut = findChars(chars, [">"], from: 0)
    let head = Array(chars[0..<(cut ?? chars.count)])
    let body = cut.map { Array(chars[($0 + 1)...]) } ?? []

    // An absent or empty `name=""` falls through to the only tool there is,
    // matching python's `(m.group(1) if m else "") or "send"`.
    let name = invokeName(head).flatMap { $0.isEmpty ? nil : $0 } ?? "send"
    var params = invokeParameters(body)
    if params.isEmpty { return nil }

    var args: OrderedDictionary<String, JSON> = [:]
    if let blob = params.removeValue(forKey: "arguments") {
        // The usual Claude shape: ONE `arguments` parameter holding the whole
        // JSON object. A non-object (or unreadable) blob contributes nothing
        // rather than being coerced — the split parameters below may still carry
        // the call.
        if let parsed = (try? JSON.parse(blob))?.asObject { args = parsed }
    }
    // Remaining `<parameter name="x">` pairs are arguments too (models split them
    // either way). JSON-decode each value so objects/ints survive; else keep the
    // text — `self` is not JSON but it is exactly what `reply_to` wants.
    //
    // A split parameter OVERWRITES a key of the same name from the `arguments`
    // blob, the opposite of the taught envelope's precedence: there `arguments` is
    // the nested slot and the strays are the model's slip, while here both are
    // parameters of the same tag and the later one is its correction.
    for (k, v) in params {
        args[k] = (try? JSON.parse(v)) ?? .string(v)
    }
    if args.isEmpty { return nil }
    return ToolCall(id: mintCallId(), name: name, arguments: .object(args))
}

/// The `name="…"` attribute value from an opening tag's attributes, or nil.
///
/// Python's `re.compile(r'name\s*=\s*"([^"]*)"').search(head)` — a SEARCH, so it
/// finds the attribute wherever it sits, and a candidate that does not complete
/// the shape (`filename` with no `=`, an unterminated quote) is skipped exactly as
/// the regex engine would retry one character further on.
private func invokeName(_ head: [Character]) -> String? {
    let key = Array("name")
    var from = 0
    while let pos = findChars(head, key, from: from) {
        from = pos + 1
        var i = skipWS(head, pos + key.count)
        guard i < head.count, head[i] == "=" else { continue }
        i = skipWS(head, i + 1)
        guard i < head.count, head[i] == "\"" else { continue }
        i += 1
        guard let close = findChars(head, ["\""], from: i) else { continue }
        return String(head[i..<close])
    }
    return nil
}

/// Every `<parameter name="k">v</parameter>` in the body, in document order, with
/// each value stripped.
///
/// Python's `re.compile(r'<parameter\s+name\s*=\s*"([^"]*)"\s*>(.*?)</parameter>',
/// re.DOTALL).findall(body)` collected into a dict, so: the value is
/// NON-GREEDY (it ends at the first `</parameter>`), it may span newlines, a
/// tag that does not complete the shape is skipped rather than ending the scan,
/// and a REPEATED key keeps its first position while taking the last value.
private func invokeParameters(_ body: [Character]) -> OrderedDictionary<String, String> {
    let openTag = Array("<parameter")
    let nameKey = Array("name")
    let closeTag = Array("</parameter>")
    var out: OrderedDictionary<String, String> = [:]
    var from = 0
    while let pos = findChars(body, openTag, from: from) {
        // A failed match advances by ONE, exactly as the regex engine retries, so
        // the next `<parameter` further along is still found.
        from = pos + 1
        var i = pos + openTag.count
        let afterWS = skipWS(body, i)
        guard afterWS > i else { continue }  // `\s+` — at least one space
        i = afterWS
        guard matchesAt(body, nameKey, i) else { continue }
        i = skipWS(body, i + nameKey.count)
        guard i < body.count, body[i] == "=" else { continue }
        i = skipWS(body, i + 1)
        guard i < body.count, body[i] == "\"" else { continue }
        guard let quoteEnd = findChars(body, ["\""], from: i + 1) else { continue }
        let key = String(body[(i + 1)..<quoteEnd])
        i = skipWS(body, quoteEnd + 1)
        guard i < body.count, body[i] == ">" else { continue }
        guard let close = findChars(body, closeTag, from: i + 1) else { continue }
        out[key] = String(body[(i + 1)..<close])
            .trimmingCharacters(in: .whitespacesAndNewlines)
        from = close + closeTag.count
    }
    return out
}
