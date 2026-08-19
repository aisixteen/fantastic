// The parsed CALL — the one shape every envelope reduces to — plus the two
// pieces of machinery every envelope's parser needs: a call-id minter and the
// `inline_send` translation.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`_mint_id`,
// `_inline_send`). The comments are part of the port: each records a measured
// failure, and it is the only thing that stops the behaviour being "simplified"
// away later.

import FantasticJSON
import Foundation
import OrderedCollections

/// One tool call read out of a completion: `{id, name, arguments}`.
///
/// `name` is almost always `send` — it is the only tool there is — but a model
/// that compressed verb and tool into one name is translated by the parsers
/// rather than corrected here, so `name` is whatever survived that translation.
struct ToolCall: Sendable, Equatable {
    /// Correlation id, minted locally (no provider hands us one — the call is
    /// plain text the model typed).
    let id: String
    /// The tool the model named.
    let name: String
    /// The call's arguments object. Always an object by the time it lands here.
    let arguments: JSON
}

/// Mint a call id. Mirrors python's `f"call_{secrets.token_hex(4)}"` — 8 hex
/// characters from the system CSPRNG.
func mintCallId() -> String {
    String(format: "call_%08x", UInt32.random(in: UInt32.min...UInt32.max))
}

/// A `send` whose payload was written FLAT among the args.
///
/// `target_id` names the destination and `reply_to` says where the answer goes —
/// both are routing, so they stay at the top level. Everything else is the
/// payload.
///
/// `target` arrives as a JSON value (it is read straight off the model's args),
/// so a non-string target is a no-call rather than a coerced guess.
func inlineSend(target: JSON, args: OrderedDictionary<String, JSON>) -> ToolCall? {
    guard let raw = target.asString else { return nil }
    let id = raw.trimmingCharacters(in: .whitespacesAndNewlines)
    if id.isEmpty { return nil }
    var payload: OrderedDictionary<String, JSON> = [:]
    for (k, v) in args where k != "target_id" && k != "reply_to" {
        payload[k] = v
    }
    var out: OrderedDictionary<String, JSON> = [:]
    out["target_id"] = .string(id)
    out["payload"] = .object(payload)
    // PRESENCE, not truthiness: `reply_to: null` is the model SAYING "discard
    // the answer", which is a different instruction from leaving the key out
    // (that one defaults to self). Read the dictionary directly — `args["x"]` on
    // a `JSON` returns `.null` for a MISSING key, so a subscript cannot tell the
    // two apart and would silently invert the default.
    if let replyTo = args["reply_to"] {
        out["reply_to"] = replyTo
    }
    return ToolCall(id: mintCallId(), name: "send", arguments: .object(out))
}
