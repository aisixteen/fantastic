// The special-token channel envelope's body: `call:<name>{k: v, …}`.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`_CHANNEL_HEAD`,
// `parse_channel_call`). The value grammar the head hands off to lives in
// `ChannelGrammar.swift`; the envelope's own delimiters and the reason it exists
// are documented at `CHANNEL_OPEN` in `Envelopes.swift`.
//
// TWO body shapes reach us, because a model compresses our ONE universal tool:
//
//   call:send{target_id:<|"|>x<|"|>,payload:{…}}   — our taught tool, in this syntax
//   call:<target>.<verb>{…}                       — the same thing, compressed
//
// The second maps onto `send(target_id: <target>, payload: {"type": <verb>, …})`.
// `reply_to` has no slot and needs none: it defaults to SELF, so the reply
// arrives as the next turn, which is what a loop wants.
//
// Still raw prompt-and-parse — plain text the model typed. No `tools=[]`, no
// `tool_choice`, no native tool_use block; the provider is untouched.

import FantasticJSON
import Foundation
import OrderedCollections

/// The `call:<name>` head of a channel body, and the index just past it.
///
/// Python's anchored `\s*call\s*:\s*([^\s{]+)\s*` — hand-written, because the
/// port carries no regex dependency (see `Scanning.swift`).
func matchChannelHead(_ chars: [Character]) -> (name: String, end: Int)? {
    var i = skipWS(chars, 0)
    for expected in "call" {
        guard i < chars.count, chars[i] == expected else { return nil }
        i += 1
    }
    i = skipWS(chars, i)
    guard i < chars.count, chars[i] == ":" else { return nil }
    i = skipWS(chars, i + 1)
    let start = i
    while i < chars.count, !chars[i].isWhitespace, chars[i] != "{" { i += 1 }
    guard i > start else { return nil }  // `[^\s{]+` — at least one character
    return (String(chars[start..<i]), skipWS(chars, i))
}

/// Parse one `call:<name>{…}` body into the universal `send` call.
func parseChannelCall(_ block: String) -> ToolCall? {
    let chars = Array(block)
    guard let head = matchChannelHead(chars) else { return nil }
    guard let parsed = parseChannelObject(chars, head.end) else {
        // Malformed args are NOT repaired — a parser that guesses what the model
        // meant is measuring its own guess. Drop the call; the model gets no
        // reply and can restate it. Dropping is not silence: the envelope stays
        // in `TOOL_ENVELOPES`, so `malformedCalls` reports it with the CHANNEL
        // hint (measured 2026-08-06: answering a single-quoted channel body with
        // a lecture about escaping JSON taught the wrong lesson).
        return nil
    }
    let name = head.name
    let args = parsed.value

    // THE ARGS DECIDE, not the name. A model that already wrote `target_id` has
    // written canonical `send` arguments, and any dotted prefix on the name is a
    // NAMESPACE it added, not a target. Measured 2026-08-04: gemma-4-12B-it
    // emitted
    //   call:kernel_state.send{"payload":{"type":"reflect"},"target_id":"kernel_state"}
    // Reading the prefix as the target turned that into
    // `send([Item(kernel_state, {"type":"send", …})])[0].result` and came back
    // `{"error": "kernel_state: unknown type 'send'"}` eight times in one run.
    //
    // Every lookup below tests PRESENCE on the dictionary, deliberately: a `JSON`
    // subscript answers `.null` for a MISSING key, so `args["payload"].isNull`
    // would also translate a call whose payload the model wrote as explicit null.
    if let target = args["target_id"] {
        if args["payload"] != nil {
            return ToolCall(id: mintCallId(), name: "send", arguments: .object(args))
        }
        // target_id, no payload, but a `type` sitting among the args: the payload
        // is INLINE — the model flattened one level instead of nesting. Measured
        // 2026-08-06, gemma-4-12B emitted
        //   call:kernel_state{type: 'reflect', target_id: 'readings'}
        // meaning "reflect readings". Before this branch the name was not dotted,
        // so the verb was looked for only in the name, found nowhere, and a
        // complete instruction was discarded. `type` beats a dotted verb here for
        // the reason the block below already gives: THE ARGS DECIDE.
        if args["type"] != nil {
            return inlineSend(target: target, args: args)
        }
        // target_id but NO payload: the VERB is still in the name and must not be
        // dropped. Measured 2026-08-04 —
        // `call:kernel_state.reflect{"target_id":"transcripts"}` means "reflect
        // transcripts". An earlier revision of this branch returned a send with no
        // payload at all; routing dispatched `None` and the handler raised
        // `'NoneType' object has no attribute 'get'`, three times in one run.
        if let dotted = splitDotted(name) {
            var payload: OrderedDictionary<String, JSON> = ["type": .string(dotted.verb)]
            for (k, v) in args where k != "target_id" { payload[k] = v }
            var out: OrderedDictionary<String, JSON> = [:]
            out["target_id"] = target
            out["payload"] = .object(payload)
            return ToolCall(id: mintCallId(), name: "send", arguments: .object(out))
        }
        // No payload and no verb anywhere — there is no call here to make.
        return nil
    }
    if let dotted = splitDotted(name) {
        // `type` FIRST, then every arg — so an explicit `type` among the args
        // overwrites the name-derived one while keeping its position, exactly as
        // python's `payload = {"type": verb}; payload.update(args)` does.
        var payload: OrderedDictionary<String, JSON> = ["type": .string(dotted.verb)]
        for (k, v) in args { payload[k] = v }
        var out: OrderedDictionary<String, JSON> = [:]
        out["target_id"] = .string(dotted.target)
        out["payload"] = .object(payload)
        return ToolCall(id: mintCallId(), name: "send", arguments: .object(out))
    }
    if args.isEmpty { return nil }
    // No target_id and an undotted name, but a `type` among the args: then the
    // NAME is the agent and the args are its payload — `call:kernel_state{type:
    // 'reflect'}` is `send('kernel_state', {'type':'reflect'})`. Without this the
    // call kept the agent's name in `name` where nothing reads it, arrived with no
    // `target_id`, and earned a `no target_id` error for a request that was
    // perfectly clear.
    if args["type"] != nil {
        return inlineSend(target: .string(name), args: args)
    }
    return ToolCall(id: mintCallId(), name: name, arguments: .object(args))
}

/// A compressed `<target>.<verb>` name split at its FIRST `.`, or nil when the
/// name carries no dot. Python's `name.partition(".")`, which keeps `a.b.c`'s
/// verb as `b.c`.
private func splitDotted(_ name: String) -> (target: String, verb: String)? {
    guard let dot = name.firstIndex(of: ".") else { return nil }
    return (String(name[name.startIndex..<dot]), String(name[name.index(after: dot)...]))
}
