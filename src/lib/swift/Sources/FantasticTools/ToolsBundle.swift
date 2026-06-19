// LLM tool registry bundle.
//
// Mirrors Rust's `fantastic-tools::ToolsBundle`. Maintains a
// process-global registry mapping tool name → {agent_id, verb,
// description, parameters_schema, sender}. LLM-using bundles read
// `list_for_llm` before every model call; dispatch routes through
// `kernel.send(entry.agent_id, ...)` — "send IS the tool call".
//
// Wire shape parity with the Rust bundle:
//   reflect              → {id, sentence, kind: "tools", tool_count, verbs}
//   register             → {ok: true, name}
//   unregister           → {ok: true} | {error, reason: "not_found"}
//   unregister_by_sender → {ok: true, removed, sender}
//   clear                → {ok: true, removed}
//   list                 → {tools: [full entries], count}
//   list_for_llm         → {tools: [{name, description, parameters}]}
//   dispatch             → reply from kernel.send(entry.agent_id, ...)

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "tools.tools"

// ── Tool entry + registry ──────────────────────────────────────────

public struct ToolEntry: Sendable {
    public let name: String
    public let agentId: AgentId
    public let verb: String?
    public let description: String
    public let parametersSchema: JSON
    public let sender: AgentId

    public init(
        name: String,
        agentId: AgentId,
        verb: String? = nil,
        description: String,
        parametersSchema: JSON,
        sender: AgentId
    ) {
        self.name = name
        self.agentId = agentId
        self.verb = verb
        self.description = description
        self.parametersSchema = parametersSchema
        self.sender = sender
    }
}

private let toolsLock = NSLock()
nonisolated(unsafe) private var tools: [String: ToolEntry] = [:]

public func register(_ entry: ToolEntry) {
    toolsLock.lock()
    defer { toolsLock.unlock() }
    tools[entry.name] = entry
}

@discardableResult
public func unregister(name: String) -> Bool {
    toolsLock.lock()
    defer { toolsLock.unlock() }
    return tools.removeValue(forKey: name) != nil
}

@discardableResult
public func unregisterBySender(sender: AgentId) -> Int {
    toolsLock.lock()
    defer { toolsLock.unlock() }
    let before = tools.count
    tools = tools.filter { $0.value.sender != sender }
    return before - tools.count
}

@discardableResult
public func clear() -> Int {
    toolsLock.lock()
    defer { toolsLock.unlock() }
    let n = tools.count
    tools.removeAll()
    return n
}

public func snapshot() -> [ToolEntry] {
    toolsLock.lock()
    defer { toolsLock.unlock() }
    return tools.values.sorted { $0.name < $1.name }
}

public func lookup(_ name: String) -> ToolEntry? {
    toolsLock.lock()
    defer { toolsLock.unlock() }
    return tools[name]
}

public func count() -> Int {
    toolsLock.lock()
    defer { toolsLock.unlock() }
    return tools.count
}

// ── Bundle ─────────────────────────────────────────────────────────

public struct ToolsBundle: AgentBundle {
    public let name = "tools"

    public init() {}

    public var readme: String? {
        "tools — LLM tool registry. Register via `kernel.send(\"tools\", {register, name, agent_id, description, parameters_schema})`; LLM-using bundles pull `list_for_llm` before every model call."
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        switch verb {
        case "reflect":
            return reflect(agentId: agentId)
        case "boot":
            return .object(["ok": .bool(true)])
        case "shutdown":
            return .object(["ok": .bool(true)])
        case "register":
            return registerVerb(payload: payload)
        case "unregister":
            return unregisterVerb(payload: payload)
        case "unregister_by_sender":
            return unregisterBySenderVerb(payload: payload)
        case "clear":
            return .object([
                "ok": .bool(true),
                "removed": .integer(Int64(clear())),
            ])
        case "list":
            return listVerb()
        case "list_for_llm":
            return listForLLMVerb()
        case "dispatch":
            return await dispatchVerb(kernel: kernel, payload: payload)
        default:
            return .object([
                "error": .string("unknown verb \(verb)"),
                "reason": .string("unknown_verb"),
            ])
        }
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        _ = clear()
    }

    // MARK: - Verbs

    private func reflect(agentId: AgentId) -> JSON {
        return [
            "id": .string(agentId.value),
            "sentence": .string(
                "Tool registry for LLM tool calling. Maps name → {agent_id, verb, schema, sender}; LLM-using bundles read list_for_llm before every model call. Dispatch is kernel.send(entry.agent_id, ...)."
            ),
            "kind": .string("tools"),
            "tool_count": .integer(Int64(count())),
            "verbs": [
                "reflect": "Identity + tool_count.",
                "register":
                    "args: name, agent_id, verb?, description, parameters_schema, sender?.",
                "unregister": "args: name.",
                "unregister_by_sender": "args: sender. Drops every entry whose sender matches.",
                "clear": "Drops every entry.",
                "list": "Returns {tools, count}.",
                "list_for_llm": "Returns {tools: [{name, description, parameters}]}.",
                "dispatch":
                    "args: name, arguments. Routes to kernel.send(entry.agent_id, {type: verb_or_name, ...arguments}).",
            ] as JSON,
            "emits": [:] as JSON,
        ] as JSON
    }

    private func registerVerb(payload: JSON) -> JSON {
        guard let name = payload["name"].asString else {
            return .object([
                "error": .string("register requires name"),
                "reason": .string("invalid_args"),
            ])
        }
        guard let agentIdStr = payload["agent_id"].asString else {
            return .object([
                "error": .string("register requires agent_id"),
                "reason": .string("invalid_args"),
            ])
        }
        guard let description = payload["description"].asString else {
            return .object([
                "error": .string("register requires description"),
                "reason": .string("invalid_args"),
            ])
        }
        let schemaValue = payload["parameters_schema"]
        guard !schemaValue.isNull else {
            return .object([
                "error": .string("register requires parameters_schema"),
                "reason": .string("invalid_args"),
            ])
        }
        let schema = coerceSchema(schemaValue)
        let verb = payload["verb"].asString
        let senderStr = payload["sender"].asString ?? "anonymous"

        let entry = ToolEntry(
            name: name,
            agentId: AgentId(agentIdStr),
            verb: verb,
            description: description,
            parametersSchema: schema,
            sender: AgentId(senderStr)
        )
        register(entry)
        return .object([
            "ok": .bool(true),
            "name": .string(name),
        ])
    }

    private func unregisterVerb(payload: JSON) -> JSON {
        guard let name = payload["name"].asString else {
            return .object([
                "error": .string("unregister requires name"),
                "reason": .string("invalid_args"),
            ])
        }
        if unregister(name: name) {
            return .object([
                "ok": .bool(true),
                "name": .string(name),
            ])
        }
        return .object([
            "error": .string("no tool named \"\(name)\""),
            "reason": .string("not_found"),
        ])
    }

    private func unregisterBySenderVerb(payload: JSON) -> JSON {
        guard let senderStr = payload["sender"].asString else {
            return .object([
                "error": .string("unregister_by_sender requires sender"),
                "reason": .string("invalid_args"),
            ])
        }
        let removed = unregisterBySender(sender: AgentId(senderStr))
        return .object([
            "ok": .bool(true),
            "removed": .integer(Int64(removed)),
            "sender": .string(senderStr),
        ])
    }

    private func listVerb() -> JSON {
        var rows: [JSON] = []
        for e in snapshot() {
            rows.append(
                .object([
                    "name": .string(e.name),
                    "agent_id": .string(e.agentId.value),
                    "verb": e.verb.map { .string($0) } ?? .null,
                    "description": .string(e.description),
                    "parameters_schema": e.parametersSchema,
                    "sender": .string(e.sender.value),
                ]))
        }
        return .object([
            "tools": .array(rows),
            "count": .integer(Int64(rows.count)),
        ])
    }

    private func listForLLMVerb() -> JSON {
        var rows: [JSON] = []
        for e in snapshot() {
            rows.append(
                .object([
                    "name": .string(e.name),
                    "description": .string(e.description),
                    "parameters": e.parametersSchema,
                ]))
        }
        return .object(["tools": .array(rows)])
    }

    private func dispatchVerb(kernel: Kernel, payload: JSON) async -> JSON {
        guard let name = payload["name"].asString else {
            return .object([
                "error": .string("dispatch requires name"),
                "reason": .string("invalid_args"),
            ])
        }
        guard let entry = lookup(name) else {
            return .object([
                "error": .string("no tool named \"\(name)\""),
                "reason": .string("tool_not_found"),
            ])
        }
        let argsValue = payload["arguments"]
        guard case var .object(args) = argsValue else {
            if argsValue.isNull {
                let verb = entry.verb ?? entry.name
                return await kernel.send(
                    entry.agentId, .object(["type": .string(verb)]))
            }
            return .object([
                "error": .string("dispatch arguments must be an object"),
                "reason": .string("invalid_args"),
            ])
        }
        args["type"] = .string(entry.verb ?? entry.name)
        return await kernel.send(entry.agentId, .object(args))
    }

    private func coerceSchema(_ v: JSON) -> JSON {
        if case let .string(s) = v {
            if let parsed = try? JSON.parse(s) {
                return parsed
            }
        }
        return v
    }
}
