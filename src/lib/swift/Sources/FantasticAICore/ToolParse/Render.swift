// The two renderers — and they are TWO, deliberately.
//
// `renderCall` is the inverse of the parser: it writes a `{name, arguments}`
// back as the grammar's own text, so a test builds its fixtures with it and they
// cannot drift from what the parser accepts.
//
// `renderReply` writes a REPLY to the model in that same tag notation — one
// notation on the whole model↔harness path, no JSON in either direction (user,
// 2026-09-09: "align answers in the same format"). Measured why: a JSON envelope
// escapes every newline and quote, so the model could not see a read's lines,
// and (2026-09-09 program_tests) a refusal carrying a JSON-dumped list showed
// doubled backslashes that cost a run.
//
// They differ in whitespace and must not share a helper: `renderCall` puts no
// newline after an open tag, `renderReply` does; `renderReply` renders a
// top-level list as `<items>` and spans a multiline scalar across lines.

import FantasticJSON
import Foundation
import OrderedCollections

/// A call as the ONE grammar's text. `send` arguments are the parser's own shape
/// (`target_id`/`to` + routing + `payload`) and come back out inline, in the
/// routing order the parser reads them in.
func renderCall(_ name: String, _ arguments: OrderedDictionary<String, JSON>) -> String {
    var args = arguments
    if name == "send" {
        var payload: OrderedDictionary<String, JSON> = [:]
        if case .some(.object(let p)) = args.removeValue(forKey: "payload") { payload = p }
        var flat: OrderedDictionary<String, JSON> = [:]
        for key in ROUTING {
            if let v = args[key] { flat[key] = v }
        }
        for (k, v) in payload { flat[k] = v }
        args = flat
    }
    let body = args.map { renderField($0.key, $0.value) }.joined()
    return "<call name=\"\(name)\">\n\(body)</call>"
}

private func renderField(_ name: String, _ value: JSON) -> String {
    if name == "bytes" { return "<bytes>\(flatText(value))</bytes>\n" }
    switch value {
    case .object(let map):
        let inner = map.map { renderField($0.key, $0.value) }.joined()
        return "<\(name)>\(inner)</\(name)>\n"
    case .array(let items):
        let inner = items.map { renderField("item", $0) }.joined()
        return "<\(name)>\(inner)</\(name)>\n"
    default:
        return "<\(name)>\(flatText(value))</\(name)>\n"
    }
}

/// A reply to the model. A dict is one `<k>…</k>` per line; a list is its outer
/// tag with one `<item>` per element; text with newlines spans lines verbatim;
/// `null` is an empty tag.
func renderReply(_ value: JSON) -> String {
    switch value {
    case .object(let map):
        let out = map.map { renderReplyField($0.key, $0.value) }.joined()
        return trimTrailingNewlines(out)
    case .array:
        return trimTrailingNewlines(renderReplyField("items", value))
    default:
        return flatText(value)
    }
}

private func renderReplyField(_ name: String, _ value: JSON) -> String {
    switch value {
    case .object(let map):
        let inner = map.map { renderReplyField($0.key, $0.value) }.joined()
        return "<\(name)>\n\(inner)</\(name)>\n"
    case .array(let items):
        // A list of STRUCTURES nests; a list holding any scalar renders one
        // `<item>` per element. An empty list takes the first branch, as
        // python's `all(...)` over nothing is true.
        let structural = items.allSatisfy {
            if case .object = $0 { return true }
            if case .array = $0 { return true }
            return false
        }
        let inner =
            structural
            ? items.map { renderReplyField("item", $0) }.joined()
            : items.map { "<item>\(flatText($0))</item>\n" }.joined()
        return "<\(name)>\n\(inner)</\(name)>\n"
    default:
        let text = flatText(value)
        return text.contains("\n")
            ? "<\(name)>\n\(text)\n</\(name)>\n" : "<\(name)>\(text)</\(name)>\n"
    }
}

private func trimTrailingNewlines(_ s: String) -> String {
    var out = Substring(s)
    while out.hasSuffix("\n") { out = out.dropLast() }
    return String(out)
}

/// One value as the text the model reads: a string is itself (never quoted), a
/// bool is `true`/`false`, null is empty, a number is its digits.
///
/// A STRUCTURE reaching here is the one shape the notation cannot say — python
/// falls back to `str(dict)`, its own repr; compact JSON is this runtime's
/// equivalent, and both are a last resort for a list that mixes scalars with
/// objects.
private func flatText(_ v: JSON) -> String {
    switch v {
    case .null: return ""
    case .bool(let b): return b ? "true" : "false"
    case .string(let s): return s
    case .integer(let i): return String(i)
    case .double(let d): return JSON.double(d).serialize()
    default: return v.serialize()
    }
}
