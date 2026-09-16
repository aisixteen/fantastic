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
    if name == "bytes" { return "<bytes>\(callText(value))</bytes>\n" }
    switch value {
    case .object(let map):
        let inner = map.map { renderField($0.key, $0.value) }.joined()
        return "<\(name)>\(inner)</\(name)>\n"
    case .array(let items):
        let inner = items.map { renderField("item", $0) }.joined()
        return "<\(name)>\(inner)</\(name)>\n"
    default:
        return "<\(name)>\(callText(value))</\(name)>\n"
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
        return replyText(value)
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
            : items.map { "<item>\(replyText($0))</item>\n" }.joined()
        return "<\(name)>\n\(inner)</\(name)>\n"
    default:
        let text = replyText(value)
        // A LF SCALAR, not a Character: swift reads CRLF as ONE Character, so a
        // `contains("\n")` over characters says no and a CRLF-only terminal reply
        // came back on one line where python spans it.
        return text.unicodeScalars.contains("\n")
            ? "<\(name)>\n\(text)\n</\(name)>\n" : "<\(name)>\(text)</\(name)>\n"
    }
}

private func trimTrailingNewlines(_ s: String) -> String {
    var out = Substring(s)
    while out.hasSuffix("\n") { out = out.dropLast() }
    return String(out)
}

/// One REPLY value as the text the model reads — python `_reply_text`: a string
/// is itself (never quoted), a bool is `true`/`false`, null is EMPTY, and
/// everything else is python's `str(x)`.
///
/// `str(x)` is what carries the shapes the notation cannot say: a float is
/// python's repr (`1e-05`, `1e+16`, `inf`, `nan`, never `0.00001`), and a
/// structure inside a mixed list is python's container repr (`{'a': 1}`), not
/// JSON.
private func replyText(_ v: JSON) -> String {
    switch v {
    case .null: return ""
    case .bool(let b): return b ? "true" : "false"
    default: return pyStr(v)
    }
}

/// One CALL field's scalar as python's `_render_field` writes it: a bool is
/// `true`/`false`, everything else is `str(x)` — so `None` renders as the word
/// `None`, which is where this differs from a reply's empty tag.
private func callText(_ v: JSON) -> String {
    if case .bool(let b) = v { return b ? "true" : "false" }
    return pyStr(v)
}
