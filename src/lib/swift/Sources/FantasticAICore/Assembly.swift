// Prompt assembly — the shared system-prompt builder behind every Swift LLM
// backend. Mirrors Rust's `fantastic-ai-core::assembly` and the Python
// `ai_core._assemble`: the model's DEFAULT system block is a lean primer
// (id-index of the tree + bundle catalog), the agent's own self-reflect, the
// how-to for REACHING another agent, and the context-window how-to.
//
// NO agent menu (the id-index plus `reflect` is how an agent is found), and — the
// line that matters — NO STRATEGY. Both how-tos state MECHANISMS: what the
// envelope looks like, where a reply goes, how to page dropped turns back. They
// never say when to call, how many times, or when to stop. The removed
// `_SEND_HOWTO` (recoverable at 2ce23d1) did exactly that — "either call again or
// write your final answer to finish", "ORIENT FIRST" — which is the loop
// dictated, and reusing that wording because it is "tuned" would quietly void
// every emergence result taken afterwards. Pinned by
// `promptStatesTheMechanismNotAStrategy`, banned-word list included.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Render a reflect reply into a one-line `sentence  k=v  k=v` blurb.
func renderReflect(_ v: JSON) -> String {
    guard var obj = v.asObject else { return "" }
    let sentence = obj["sentence"]?.asString ?? ""
    obj["sentence"] = nil
    var parts: [String] = []
    for (k, val) in obj {
        let rendered = val.asString ?? val.serialize()
        parts.append("\(k)=\(rendered)")
    }
    let fields = parts.joined(separator: "  ")
    return "\(sentence)  \(fields)".trimmingCharacters(in: .whitespaces)
}

/// How to reach another agent, and where that send's answer goes — the block
/// that makes the dispatch path usable. Byte-for-byte the Python
/// `_SEND_TO_HOWTO` (core.py:438-448) and Rust `SEND_TO_HOWTO`.
///
/// It lands with ROUTING, never before it: a prompt that teaches an envelope
/// nothing dispatches is strictly worse than a prompt that stays quiet, because
/// the model then writes calls, receives no reply, and — measured repeatedly on
/// gemma-4-12B — invents the replies it never got.
///
/// Three facts, no strategy: the envelope's shape, that `reply_to` defaults to
/// the sender (so an omitted key is a chain, not a discard), and that a turn
/// tagged `[reply to <id> <verb>]` is an answer rather than a human's message. When
/// to call and when to stop are the model's to decide.
let SEND_TO_HOWTO = """
    ## Reaching other agents
    Everything is an agent with a string id. `send(target_id, payload)` reaches any of them —
    the payload's `type` names the verb; `{type:'reflect'}` returns that agent's verbs and
    state. To send, emit exactly this, alone on its line:
    <tool_call>{"name":"send","arguments":{"target_id":"<id>","payload":{"type":"<verb>"},"reply_to":"self"}}</tool_call>
    `reply_to` is where that send's reply goes. It defaults to you — leave it out and the
    answer comes back as your next message. Name another agent's id to hand the answer to
    them instead, or `null` to discard it. Text outside the tag is your message to whoever
    asked. A message tagged `[reply to <pid> · <id> <verb>]` is the reply to the send you issued
    naming that agent and that verb — replies can lag a turn behind when several calls are
    in flight, so match the tag before assuming a call was lost and re-issuing it.
    """

/// The context-window how-to, appended to the DEFAULT system block. Context is
/// the backend's (it owns the conversation), so it teaches the recall /
/// context_status affordances — the same footing as `SEND_TO_HOWTO`, which
/// teaches the send envelope the backend also owns. Byte-for-byte the Python
/// `_CONTEXT_HOWTO` (and Rust `CONTEXT_HOWTO`).
let CONTEXT_HOWTO = """
    ## Context window & compaction
    Your live context is finite. When the conversation outgrows it, the system COMPACTS your
    view: older turns are summarized or elided and a `[context-notice]` turn is inserted. The
    DURABLE transcript is ALWAYS whole — compaction only shapes what you see right now, and
    nothing is ever truly lost.
    - To page dropped turns back on demand: `send('<your_own_id>', {type:'recall', query?, limit?})`.
      `query` substring-filters your full history; `limit` caps the page. Use it whenever you
      need a detail from earlier that isn't in your current view.
    - When a compaction notice lands and the dropped span held durable facts (names, decisions,
      preferences), persist them to a memory agent (reflect the tree to find one) via the send
      tool — that is how they survive beyond this window.
    - To inspect your budget + the last compaction + your own last reaction:
      `send('<your_own_id>', {type:'context_status'})`.
    """
