// RAW tool-call parsing — the shared helpers a CALLER uses to read a
// completion, and the compaction-reaction reader uses to detect prior calls.
//
// Fantastic NEVER uses a provider's native tool API, and this backend runs ONE
// provider pass per `send` (no tool loop). Tool-calling is the caller's: an
// authored agent teaches its `send` convention via `system_prompt`, then reads
// the completed text with `extractToolCalls`. Providers stay pure raw-text
// streamers. Mirrors Rust `tool_parse` / Python `tool_parse` (both trimmed to
// `parse_one` + `extract_tool_calls`).
//
// The envelope (Hermes-style — widely trained, unambiguous):
//
//   <tool_call>{"name": "send", "arguments": {"target_id": "...", "payload": {...}}}</tool_call>
//
// Text OUTSIDE the tags is content shown to the user. Tags may repeat.

import FantasticJSON
import Foundation
import OrderedCollections

let TOOL_OPEN = "<tool_call>"
let TOOL_CLOSE = "</tool_call>"

/// Serialize a call into the envelope — used to teach the convention by
/// example, or when a caller records an assistant turn as text.
func renderToolCall(name: String, arguments: JSON) -> String {
    let obj: JSON = .object(["name": .string(name), "arguments": arguments])
    return TOOL_OPEN + obj.serialize() + TOOL_CLOSE
}

/// Parse the JSON between one tag pair into `(name, arguments)`.
///
/// Lenient (tiny models drift): accepts `{name,arguments}`, a `tool` alias for
/// `name`, a flattened object (remaining keys become arguments), and a
/// double-encoded (stringified) arguments value. Returns nil on unparseable
/// JSON — the caller surfaces the raw text as content so nothing is lost.
func parseOneToolCall(_ inner: String) -> (String, JSON)? {
    let trimmed = inner.trimmingCharacters(in: .whitespacesAndNewlines)
    guard let v = try? JSON.parse(trimmed), let obj = v.asObject else { return nil }
    let name = v["name"].asString ?? v["tool"].asString ?? "send"
    let argsField = v["arguments"]
    var args: JSON
    if let s = argsField.asString {
        args = (try? JSON.parse(s)) ?? .object([:])
    } else if argsField.asObject != nil {
        args = argsField
    } else {
        // flattened: remaining keys become the arguments object
        var m: OrderedDictionary<String, JSON> = [:]
        for (k, vv) in obj where k != "name" && k != "tool" && k != "arguments" {
            m[k] = vv
        }
        args = .object(m)
    }
    if args.asObject == nil { args = .object([:]) }
    return (name, args)
}

/// Non-streaming: pull every finalized `<tool_call>` out of a complete string.
/// Used by the durable-history reader (the compaction reaction).
func extractToolCalls(_ text: String) -> [(String, JSON)] {
    var out: [(String, JSON)] = []
    var rest = Substring(text)
    while let open = rest.range(of: TOOL_OPEN) {
        let afterOpen = rest[open.upperBound...]
        guard let close = afterOpen.range(of: TOOL_CLOSE) else { break }
        let inner = String(afterOpen[afterOpen.startIndex..<close.lowerBound])
        if let c = parseOneToolCall(inner) { out.append(c) }
        rest = afterOpen[close.upperBound...]
    }
    return out
}
