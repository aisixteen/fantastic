// The parsed CALL — the one shape every envelope reduces to — plus the call-id
// minter every envelope's parser needs.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`_mint_id`). The
// comments are part of the port: each records why the behaviour is what it is.

import FantasticJSON
import Foundation

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
    /// The stream part this call was previewed as — python's `call["stream_id"]`,
    /// stamped by `routeReply`'s match. Never set by a parser.
    var streamId: String? = nil
}

/// Mint a call id. Mirrors python's `f"call_{secrets.token_hex(4)}"` — 8 hex
/// characters from the system CSPRNG.
func mintCallId() -> String {
    String(format: "call_%08x", UInt32.random(in: UInt32.min...UInt32.max))
}
