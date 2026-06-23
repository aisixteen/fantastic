// RAW tool-call parsing — the ONE shared layer that owns tool-calling.
//
// Fantastic NEVER uses a provider's native tool API. Providers are pure
// raw-text streamers (`.token` only); THIS module wraps that stream and
// extracts the `send` tool calls from the text. Used by `AIBackend` for every
// backend. Mirrors Rust `tool_parse` / Python `tool_parse`.
//
// The envelope (Hermes-style — widely trained, unambiguous, stream-friendly):
//
//   <tool_call>{"name": "send", "arguments": {"target_id": "...", "payload": {...}}}</tool_call>
//
// Text OUTSIDE the tags is content shown to the user. Tags may repeat. The
// parser yields the SAME `AIChunk`s the loop already consumes; a pre-formed
// `.toolCall` (e.g. from a test) passes through untouched — real providers
// never emit one.

import FantasticJSON
import Foundation
import OrderedCollections

let TOOL_OPEN = "<tool_call>"
let TOOL_CLOSE = "</tool_call>"

private func mintToolId() -> String {
    "call_" + UUID().uuidString.prefix(8).lowercased()
}

/// Serialize a call into the envelope — used in the prompt example and when
/// persisting an assistant turn so the model re-reads its own call as text.
func renderToolCall(name: String, arguments: JSON) -> String {
    let obj: JSON = .object(["name": .string(name), "arguments": arguments])
    return TOOL_OPEN + obj.serialize() + TOOL_CLOSE
}

/// Shape a parsed call into the OpenAI-style tool-call object the loop expects
/// (`{id, type:"function", function:{name, arguments}}`).
private func openAIToolCall(name: String, arguments: JSON) -> JSON {
    .object([
        "id": .string(mintToolId()),
        "type": .string("function"),
        "function": .object(["name": .string(name), "arguments": arguments]),
    ])
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

/// Longest suffix of `s` that is a proper prefix of TOOL_OPEN (a tag possibly
/// split across chunks) — held back rather than emitted as content.
private func partialOpenLen(_ s: String) -> Int {
    let maxK = min(s.count, TOOL_OPEN.count - 1)
    if maxK <= 0 { return 0 }
    for k in stride(from: maxK, through: 1, by: -1) where s.hasSuffix(String(TOOL_OPEN.prefix(k))) {
        return k
    }
    return 0
}

/// Wrap a provider stream → content tokens + finalized tool-calls. Buffers
/// across chunks (a tag may split mid-token); malformed JSON inside a tag (or
/// an unterminated tag at EOF) is surfaced as content, never dropped. A
/// pre-formed `.toolCall` passes through (flushing buffered content first).
func parseToolCalls(
    _ inner: AsyncThrowingStream<AIChunk, Error>
) -> AsyncThrowingStream<AIChunk, Error> {
    AsyncThrowingStream { continuation in
        let task = Task {
            var buf = ""
            var inside = false

            func drainBuf() {
                while true {
                    if !inside {
                        if let open = buf.range(of: TOOL_OPEN) {
                            let before = String(buf[buf.startIndex..<open.lowerBound])
                            if !before.isEmpty { continuation.yield(.token(before)) }
                            buf = String(buf[open.upperBound...])
                            inside = true
                        } else {
                            let hold = partialOpenLen(buf)
                            let end = buf.index(buf.endIndex, offsetBy: -hold)
                            let emit = String(buf[buf.startIndex..<end])
                            if !emit.isEmpty { continuation.yield(.token(emit)) }
                            buf = String(buf[end...])
                            break
                        }
                    } else if let close = buf.range(of: TOOL_CLOSE) {
                        let inner = String(buf[buf.startIndex..<close.lowerBound])
                        buf = String(buf[close.upperBound...])
                        inside = false
                        if let (name, args) = parseOneToolCall(inner) {
                            continuation.yield(.toolCall(openAIToolCall(name: name, arguments: args)))
                        } else {
                            continuation.yield(.token(TOOL_OPEN + inner + TOOL_CLOSE))
                        }
                    } else {
                        break  // need more to close the tag
                    }
                }
            }

            do {
                for try await chunk in inner {
                    switch chunk {
                    case .toolCall(let c):
                        if !inside && !buf.isEmpty {
                            continuation.yield(.token(buf))
                            buf = ""
                        }
                        continuation.yield(.toolCall(c))
                    case .token(let t):
                        buf += t
                        drainBuf()
                    }
                }
                // flush
                if inside {
                    continuation.yield(.token(TOOL_OPEN + buf))
                } else if !buf.isEmpty {
                    continuation.yield(.token(buf))
                }
                continuation.finish()
            } catch {
                continuation.finish(throwing: error)
            }
        }
        continuation.onTermination = { _ in task.cancel() }
    }
}
