// durable_memory — a modeless durable KV agent.
//
// Swift mirror of the canonical python `durable_memory` bundle (canonical; on
// disagreement this file is wrong).
//
// One store, one discipline: keys in, values out. What the store is FOR lives in
// the agent's `description`, where every agent already says it — not in a
// bundle-private `mode` enum. (An earlier design carried `mode: mem|data|chat`;
// the verbs were identical in all three, so the modes were dropped.)
//
// NO FS OF ITS OWN. The record names `file_bridge_id`, and every byte moves
// through kernel sends to that leg over the AGNOSTIC STREAM CHANNEL —
// `write_stream` / `read_stream`, raw bytes (the binary channel), no string body
// anywhere. This bundle never touches FileManager; a store with no leg wired
// FAILFASTS on write (no silent RAM) and reads empty.
//
// YAML is the intermediate: the live state is one dict, serialized to YAML bytes
// on every write, parsed on read. Disk-is-truth — read fresh each call, no cache.
//
// Keys are flat namespaced strings (dotted convention: `domain.subject.attribute`).
// Values are arbitrary JSON. `get` is a RESERVED kernel verb, so the read verb is
// `read`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Yams

/// Where the store lands inside the bound leg's root.
private let STATE_PATH = "state.yaml"

public struct DurableMemoryBundle: AgentBundle {
    public let name = "durable_memory"

    public init() {}

    public var readme: String? { Self.readmeText }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws
        -> JSON?
    {
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent \(agentId)")])
        }
        switch payload["type"].asString ?? "" {
        case "reflect":
            return await reflectReply(agent, kernel: kernel)
        case "boot":
            return .null
        case "read":
            guard let key = payload["key"].asString, !key.isEmpty else {
                return .object(["error": .string("durable_memory.read: key (str) required")])
            }
            let doc = await load(agent, kernel: kernel)
            return .object(["key": .string(key), "value": doc[key] ?? .null])
        case "keys":
            let doc = await load(agent, kernel: kernel)
            return .object(["keys": .array(doc.keys.sorted().map(JSON.string))])
        case "set":
            guard let key = payload["key"].asString, !key.isEmpty else {
                return .object(["error": .string("durable_memory.set: key (str) required")])
            }
            guard let value = payload.asObject?["value"] else {
                return .object(["error": .string("durable_memory.set: value required")])
            }
            var doc = await load(agent, kernel: kernel)
            doc[key] = value
            if let err = await persist(agent, kernel: kernel, doc: doc, verb: "set") {
                return err
            }
            return .object(["key": .string(key), "ok": .bool(true)])
        case "append":
            guard let key = payload["key"].asString, !key.isEmpty else {
                return .object(["error": .string("durable_memory.append: key (str) required")])
            }
            guard let value = payload.asObject?["value"] else {
                return .object(["error": .string("durable_memory.append: value required")])
            }
            var doc = await load(agent, kernel: kernel)
            var cur: [JSON]
            switch doc[key] {
            case nil, .some(.null): cur = []
            case .some(.array(let a)): cur = a
            default:
                return .object([
                    "error": .string(
                        "durable_memory.append: key \"\(key)\" holds a non-list — "
                            + "`set` it to a list first, or pick another key")
                ])
            }
            cur.append(value)
            doc[key] = .array(cur)
            if let err = await persist(agent, kernel: kernel, doc: doc, verb: "append") {
                return err
            }
            return .object(["key": .string(key), "ok": .bool(true), "length": .integer(Int64(cur.count))])
        case "delete":
            guard let key = payload["key"].asString, !key.isEmpty else {
                return .object(["error": .string("durable_memory.delete: key (str) required")])
            }
            var doc = await load(agent, kernel: kernel)
            let existed = doc.removeValue(forKey: key) != nil
            if existed {
                if let err = await persist(agent, kernel: kernel, doc: doc, verb: "delete") {
                    return err
                }
            }
            return .object(["key": .string(key), "deleted": .bool(existed)])
        case let other:
            return .object(["error": .string("durable_memory: unknown type \"\(other)\"")])
        }
    }

    // ── persistence (DIRECTLY through file_bridge_id, binary stream channel) ──

    private func fileBridgeId(_ agent: Agent) -> String? {
        agent.metaValue(forKey: "file_bridge_id")?.asString.flatMap { $0.isEmpty ? nil : $0 }
    }

    private func emit(_ doc: [String: JSON]) -> Data {
        if doc.isEmpty { return Data() }
        var anyDict: [String: Any] = [:]
        for (k, v) in doc { anyDict[k] = jsonToAny(v) }
        let text = (try? Yams.dump(object: anyDict, allowUnicode: true, sortKeys: true)) ?? ""
        return Data(text.utf8)
    }

    /// The whole store, via `read_stream` chunks off the bound leg. Missing leg,
    /// missing file, or unparseable content ⇒ [:] — reads are lenient; writes are
    /// where loss would hide, and they failfast instead.
    private func load(_ agent: Agent, kernel: Kernel) async -> [String: JSON] {
        guard let fid = fileBridgeId(agent) else { return [:] }
        var data = Data()
        var offset = 0
        // Persistence is processed from ABOVE: reading/writing your own sidecar is
        // operator bookkeeping (a side effect of your authorized verb), not a
        // lateral call — so it runs ungated, like create_agent's child-boot.
        await KernelTaskLocals.$currentSender.withValue(nil) {
            while true {
                let (r, body) = await kernel.sendWithBinary(
                    AgentId(fid),
                    .object([
                        "type": .string("read_stream"), "path": .string(STATE_PATH),
                        "offset": .integer(Int64(offset)),
                    ]), Data())
                if r["error"].asString != nil { data = Data(); break }
                if !body.isEmpty { data.append(body) }
                if r["eof"].asBool == true || body.isEmpty { break }
                guard let next = r["next_offset"].asInt, Int(next) > offset else { break }
                offset = Int(next)
            }
        }
        guard !data.isEmpty, let text = String(data: data, encoding: .utf8),
            let parsed = try? Yams.load(yaml: text), let dict = parsed as? [String: Any]
        else { return [:] }
        var doc: [String: JSON] = [:]
        for (k, v) in dict { doc[k] = anyToJSON(v) }
        return doc
    }

    /// Write the whole store as ONE truncating `write_stream` chunk to the bound
    /// leg. An unwired or refusing leg is surfaced as the verb's error — never
    /// silent RAM. Returns an error JSON, or nil on success.
    private func persist(_ agent: Agent, kernel: Kernel, doc: [String: JSON], verb: String)
        async -> JSON?
    {
        guard let fid = fileBridgeId(agent) else {
            return .object([
                "error": .string(
                    "durable_memory.\(verb): no file_bridge_id wired — writes have "
                        + "nowhere durable to land, refusing rather than keeping state in RAM")
            ])
        }
        let w = await KernelTaskLocals.$currentSender.withValue(nil) {
            await kernel.sendWithBinary(
                AgentId(fid),
                .object([
                    "type": .string("write_stream"), "path": .string(STATE_PATH),
                    "truncate": .bool(true),
                ]), emit(doc)
            ).0
        }
        guard case .object = w else {
            return .object(["error": .string("durable_memory.\(verb): sink gave no reply")])
        }
        if let err = w["error"].asString {
            var out: OrderedDictionary<String, JSON> = [
                "error": .string("durable_memory.\(verb): \(err)")
            ]
            for k in ["reason", "hint"] where !w[k].isNull { out[k] = w[k] }
            return .object(out)
        }
        return nil
    }

    private func reflectReply(_ agent: Agent, kernel: Kernel) async -> JSON {
        let doc = await load(agent, kernel: kernel)
        let sentence =
            agent.metaValue(forKey: "description")?.asString
            ?? "A durable key/value store. `set`/`read`/`append` named keys; state "
                + "survives restarts through its bound file leg."
        return .object([
            "id": .string(agent.id.value),
            "sentence": .string(sentence),
            "file_bridge_id": fileBridgeId(agent).map { JSON.string($0) } ?? .null,
            "keys": .array(doc.keys.sorted().map(JSON.string)),
            "verbs": .object([
                "read": .string("args: key:str — {key, value} (value null when absent)"),
                "set": .string("args: key:str, value:any — stores it (overwrite)"),
                "append": .string(
                    "args: key:str, value:any — appends to the LIST at key "
                        + "(created if absent; refuses a non-list)"),
                "keys": .string("no args — {keys: [...]}"),
                "delete": .string("args: key:str — {key, deleted:bool}"),
            ]),
        ])
    }

    // ── JSON ⇄ Any (for Yams) ───────────────────────────────────

    private func jsonToAny(_ j: JSON) -> Any {
        switch j {
        case .null: return NSNull()
        case .bool(let b): return b
        case .integer(let i): return Int(i)
        case .double(let d): return d
        case .string(let s): return s
        case .array(let a): return a.map { jsonToAny($0) }
        case .object(let o):
            var dict: [String: Any] = [:]
            for (k, v) in o { dict[k] = jsonToAny(v) }
            return dict
        }
    }

    private func anyToJSON(_ v: Any) -> JSON {
        if v is NSNull { return .null }
        if let b = v as? Bool { return .bool(b) }
        if let i = v as? Int { return .integer(Int64(i)) }
        if let i = v as? Int64 { return .integer(i) }
        if let d = v as? Double { return .double(d) }
        if let s = v as? String { return .string(s) }
        if let a = v as? [Any] { return .array(a.map { anyToJSON($0) }) }
        if let o = v as? [String: Any] {
            var dict: OrderedDictionary<String, JSON> = [:]
            for (k, val) in o { dict[k] = anyToJSON(val) }
            return .object(dict)
        }
        return .null
    }

    static let readmeText = """
        # durable_memory — a modeless durable key/value store

        A YAML key-value store that survives the context boundary. One mechanism,
        many uses: global or local, long- or short-term memory, durable component
        state. What the store is FOR lives in the agent's `description`.

        NO fs of its own: the record names `file_bridge_id`, and every byte moves
        through `write_stream`/`read_stream` sends to that leg (raw bytes). A store
        with no leg wired failfasts on write and reads empty.

        ## Verbs
        - `read {key}` — value at `key` (null if absent).
        - `keys {}` — the key names.
        - `set {key, value}` — upsert one key.
        - `append {key, value}` — append to the LIST at `key` (created if absent; refuses a non-list).
        - `delete {key}` — prune a key.

        ## Recipes
        - Remember a fact → `set {key:"user.name", value:"Ada"}`.
        - Grow a log → `append {key:"events", value:{...}}`.
        - Reuse keys, don't duplicate → `keys` first; use descriptive namespaced keys
          (`domain.subject.attribute`).
        """
}
