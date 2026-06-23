// Prompt assembly — the shared system-prompt builder behind every
// Swift LLM backend. Mirrors Rust's `fantastic-ai-core::assembly` and
// the Python `ai_core._assemble`: the model's system block is rebuilt
// every turn from the live substrate — a lean primer (id-index of the
// tree + bundle catalog), the agent's own self-reflect, a "menu" of
// the other agents (one line each, with verb names), and the universal
// `send` tool how-to. The single `send` tool reaches EVERY agent +
// verb, so capability is discovered, not hardcoded.

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

/// Reflect on every running agent (skip self) and collect their
/// one-line sentence + verb names — the model's "menu of capabilities".
func buildMenu(selfId: AgentId, kernel: Kernel) async -> [JSON] {
    let online = await kernel.send(
        AgentId("core"), .object(["type": .string("list_agents")]))
    guard let agents = online["agents"].asArray else { return [] }
    var items: [JSON] = []
    for a in agents {
        guard let id = a["id"].asString, id != selfId.value else { continue }
        let r = await kernel.send(AgentId(id), .object(["type": .string("reflect")]))
        let sentence = r["sentence"].asString ?? ""
        var verbNames: [JSON] = []
        if let verbs = r["verbs"].asObject {
            verbNames = verbs.keys.map { .string($0) }
        } else if let verbs = r["verbs"].asArray {
            verbNames = verbs.compactMap { $0.asString.map(JSON.string) }
        }
        items.append(
            .object([
                "id": .string(id),
                "sentence": .string(sentence),
                "verbs": .array(verbNames),
            ]))
    }
    return items
}

/// Format the menu as bullet lines for the system prompt.
func renderMenu(_ menu: [JSON]) -> String {
    if menu.isEmpty {
        return "## Available agents\n(none — only `core` and `self`)"
    }
    var lines = [
        "## Available agents (reflect on any for full verb signatures + arg shapes;"
            + " reflect `core` with readme:true for the whole-system guide)"
    ]
    for m in menu {
        let id = m["id"].asString ?? ""
        let sentence = m["sentence"].asString ?? ""
        let verbs = (m["verbs"].asArray ?? []).compactMap { $0.asString }
        let head =
            verbs.count > 10
            ? verbs.prefix(10).joined(separator: ", ") + " …"
            : verbs.joined(separator: ", ")
        let headDisplay = head.isEmpty ? "(none)" : head
        lines.append("- `\(id)` — \(sentence) — verbs: \(headDisplay)")
    }
    return lines.joined(separator: "\n")
}

/// The universal `send` tool how-to, appended to the system prompt.
let SEND_HOWTO = """
    ## How to use the `send` tool
    You have ONE tool: `send(target_id, payload)`. EVERY action goes through it.

    To CALL it, emit EXACTLY this on its own — a `<tool_call>` tag wrapping one JSON object:
    <tool_call>{"name": "send", "arguments": {"target_id": "<id>", "payload": {"type": "<verb>", ...fields}}}</tool_call>
    Rules:
    - Emit the tag verbatim; put NOTHING else on that line. You may emit several tags to
      batch calls. Any text OUTSIDE the tags is shown to the user as your message.
    - After a call, you receive a `<tool_response>...</tool_response>` with the reply. READ it,
      then either call again or write your final answer (no tag) to finish.
    - Example — list the agents:
      <tool_call>{"name": "send", "arguments": {"target_id": "core", "payload": {"type": "list_agents"}}}</tool_call>

    - ORIENT FIRST. For anything beyond the menu's verb names — especially the
      browser frontend (panels/views), persistence, or how agents are wired — read
      the full system guide in ONE call BEFORE acting:
      `send('core', {type:'reflect', readme:true})`. Don't guess the wiring — read it first.
    - To do something concrete (read a file, run python, list agents, etc.), pick
      an agent from the menu above whose verbs cover what you need, then build
      `{type:'<verb>', ...args}` and pass it as `payload`.
    - To learn an agent's full verb signatures (arg names, types):
      `send('<id>', {type:'reflect'})` returns `{verbs: {name: 'doc'}, ...}`.
    - To rebuild your menu of agents (useful right after you create one):
      `send('<your_own_id>', {type:'refresh_menu'})` — next turn shows the fresh menu.
    - NEVER claim "I don't have access" without trying the menu first. The
      send tool reaches every agent in the system.
    """

// NOTE: there is NO native tool schema. Tool-calling is RAW prompt-and-parse,
// owned by the base class: the `send` tool + its `<tool_call>` text envelope are
// taught in `SEND_HOWTO`, the model emits the call as text, and `ToolParse`
// parses it back out of the stream. Providers get NO tools array.
