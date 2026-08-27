// yaml_state — a durable YAML key-value memory agent.
//
// One bundle, instantiated as N agents. The `mode` meta ("mem" | "data")
// sets the *discipline* (and the reflect sentence) — the verbs are
// identical:
//
//   - data → current scratch-state, overwrite-in-place.
//   - mem  → durable keyed facts, accrete + prune at LLM discretion.
//
// ALL disk IO goes THROUGH a `file_bridge` AGENT (the gated fs edge — sealed /
// deny-all by default) that the LOADER discovers — this bundle never names a
// This bundle owns NO disk surface of its own and never touches FileManager: it
// `send`s `read` / `write` verbs to its provider, exactly like the Python bundle.
// provider and never computes a path. `set`/`delete`/`replace` FAIL when no store
// surface a denied write rather than losing it). Wire it to the `.fantastic`
// store (the one the loader persists records through — ONE file_bridge serves
// both): the path is store-relative `agents/<id>/state.yaml`, next to its
// `agent.json`. Disk-is-truth (read fresh each call, no cache).
//
// Keys are flat namespaced strings (dotted convention:
// `domain.subject.attribute`). Values are arbitrary JSON. Mirrors the
// canonical Python `yaml_state` bundle.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Yams

public struct YamlStateBundle: AgentBundle {
    public let name = "yaml_state"

    public init() {}

    public var readme: String? { Self.readmeText }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws
        -> JSON?
    {
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent \(agentId)")])
        }
        let verb = payload["type"].asString ?? ""
        switch verb {
        case "reflect":
            return await reflectReply(agent, kernel: kernel)
        case "boot", "shutdown":
            return .null
        case "read":
            let doc = await load(agent, kernel: kernel)
            if let key = payload["key"].asString {
                return .object(["key": .string(key), "value": doc[key] ?? .null])
            }
            return .object([
                "doc": .object(OrderedDictionary(uniqueKeysWithValues: doc.map { ($0, $1) }))
            ])
        case "keys":
            let doc = await load(agent, kernel: kernel)
            let items: [JSON] = doc.keys.sorted().map { k in
                .object(["key": .string(k), "size": .integer(Int64(valueSize(doc[k]!)))])
            }
            return .object(["keys": .array(items)])
        case "set":
            guard let key = payload["key"].asString, !key.isEmpty else {
                return .object([
                    "error": .string("yaml_state.set: key (non-empty str) required")
                ])
            }
            guard let value = payload.asObject?["value"] else {
                return .object(["error": .string("yaml_state.set: value required")])
            }
            var doc = await load(agent, kernel: kernel)
            doc[key] = value
            if let err = await persist(agent, kernel: kernel, doc: doc, verb: "set") {
                return err
            }
            return .object(["key": .string(key), "set": .bool(true)])
        case "delete":
            guard let key = payload["key"].asString, !key.isEmpty else {
                return .object([
                    "error": .string("yaml_state.delete: key (non-empty str) required")
                ])
            }
            var doc = await load(agent, kernel: kernel)
            let existed = doc.removeValue(forKey: key) != nil
            if let err = await persist(agent, kernel: kernel, doc: doc, verb: "delete") {
                return err
            }
            return .object(["key": .string(key), "deleted": .bool(existed)])
        case "replace":
            // `doc` is REQUIRED ({} clears) — a missing doc is an error, not a
            // silent clear (matches Python).
            guard let docVal = payload.asObject?["doc"] else {
                return .object([
                    "error": .string("yaml_state.replace: doc (object) required")
                ])
            }
            guard case .object(let obj) = docVal else {
                return .object([
                    "error": .string("yaml_state.replace: doc must be an object")
                ])
            }
            var doc: [String: JSON] = [:]
            for (k, v) in obj { doc[k] = v }
            if let err = await persist(agent, kernel: kernel, doc: doc, verb: "replace") {
                return err
            }
            return .object(["replaced": .bool(true), "keys": .integer(Int64(doc.count))])
        case "state_yaml":
            return .object(["yaml": .string(emit(await load(agent, kernel: kernel)))])
        default:
            return .object(["error": .string("yaml_state: unknown type '\(verb)'")])
        }
    }

    // ── persistence (THROUGH a file_bridge provider) ────────────
    //
    // ALL disk IO goes THROUGH a `file_bridge` AGENT (the gated fs edge), keyed
    // The sidecar goes THROUGH the kernel: this bundle names only itself and a
    // filename, and the loader owns where it lands.

    private func fileBridgeId(_ agent: Agent) -> String? {
        agent.metaValue(forKey: "file_bridge_id")?.asString
    }

    /// The sidecar this agent keeps. A NAME, not a path — the LOADER owns where
    /// sidecars land, so this bundle no longer computes `agents/<id>/…` and no
    /// longer names a provider of its own.
    private static let stateBlob = "state.yaml"

    private func modeOf(_ agent: Agent) -> String {
        agent.metaValue(forKey: "mode")?.asString == "mem" ? "mem" : "data"
    }

    /// Read the store THROUGH the wired provider. Unwired / missing / denied ⇒ [:].
    private func load(_ agent: Agent, kernel: Kernel) async -> [String: JSON] {
        // THROUGH THE LOADER, not a provider of our own. Operator authority:
        // reading our OWN sidecar is bookkeeping processed from above.
        let r = await KernelTaskLocals.$currentSender.withValue(nil) {
            await kernel.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("load_blob"),
                        "agent_id": .string(agent.id.value),
                        "name": .string(Self.stateBlob),
                    ]))
            ])[0].result
        }
        guard
            let text = r["content"].asString,
            !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
            let parsed = try? Yams.load(yaml: text),
            let dict = parsed as? [String: Any]
        else {
            return [:]
        }
        var doc: [String: JSON] = [:]
        for (k, v) in dict { doc[k] = anyToJSON(v) }
        return doc
    }

    private func emit(_ doc: [String: JSON]) -> String {
        if doc.isEmpty { return "" }
        var anyDict: [String: Any] = [:]
        for (k, v) in doc { anyDict[k] = jsonToAny(v) }
        return (try? Yams.dump(object: anyDict, allowUnicode: true, sortKeys: true)) ?? ""
    }

    /// Write the store THROUGH the provider; surface a denied/failed write as an
    /// error (no silent loss). Returns an error JSON, or nil on success. Error
    /// text byte-identical to Python.
    private func persist(_ agent: Agent, kernel: Kernel, doc: [String: JSON], verb: String) async
        -> JSON?
    {
        // Operator authority: persisting from above (see `load`).
        let w = await KernelTaskLocals.$currentSender.withValue(nil) {
            await kernel.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("persist_blob"),
                        "agent_id": .string(agent.id.value),
                        "name": .string(Self.stateBlob),
                        "content": .string(emit(doc)),
                    ]))
            ])[0].result
        }
        guard case .object = w else {
            return .object(["error": .string("yaml_state.\(verb): loader gave no reply")])
        }
        let reason =
            w["error"].asString ?? (w["reason"].asString == "unauthorized" ? "unauthorized" : nil)
        if let reason {
            var out: JSON = .object([
                "error": .string("yaml_state.\(verb): \(reason)")
            ])
            // Pass the provider's `hint` through (py parity) — e.g. the sealed-edge
            // denial carries the open-it recipe.
            if case .object = w, !w["hint"].isNull {
                out["hint"] = w["hint"]
            }
            return out
        }
        return nil
    }

    private func valueSize(_ j: JSON) -> Int {
        if case .string(let s) = j { return s.count }
        return j.serialize().count
    }

    private func reflectReply(_ agent: Agent, kernel: Kernel) async -> JSON {
        let mode = modeOf(agent)
        let doc = await load(agent, kernel: kernel)
        return .object([
            "id": .string(agent.id.value),
            "sentence": .string(mode == "mem" ? Self.memSentence : Self.dataSentence),
            "mode": .string(mode),
            "key_count": .integer(Int64(doc.count)),
            "file_bridge_id": fileBridgeId(agent).map { JSON.string($0) } ?? .null,
            "verbs": .object([
                "read": .string(
                    "args: key?:str. Value at key (null if absent); whole doc if key omitted."),
                "keys": .string("args: none. List keys + value sizes — the table-of-contents."),
                "set": .string(
                    "args: key:str, value:any. Upsert one key. Persisted through file_bridge_id; failfast if unwired."
                ),
                "delete": .string("args: key:str. Remove a key."),
                "replace": .string("args: doc:object. Overwrite the whole store ({} clears)."),
                "state_yaml": .string("args: none. The entire store as YAML text."),
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

    // ── model-audience strings ──────────────────────────────────

    static let memSentence =
        "Your durable memory. Facts you must remember across sessions live here "
        + "— auto-loaded into your context on boot. `set` a descriptive key the "
        + "moment the user tells you something worth keeping (a name, a preference, "
        + "a decision). Your current facts are already in your context — read them, "
        + "don't re-fetch."
    static let dataSentence =
        "Your durable scratch-state (component state, config, run params, current "
        + "selection). One value per key, overwrite-in-place; auto-loaded into your "
        + "context on boot."

    static let readmeText = """
        # yaml_state — durable state & memory agent

        A YAML key-value store that survives the context boundary. One mechanism, many
        uses: global or local, long- or short-term memory, and durable component state.
        The `mode` meta picks the discipline (same verbs either way):
        - `data` — current scratch-state (component state, config, run params, selection); overwrite-in-place.
        - `mem` — durable facts to remember (names, preferences, decisions); accrete keyed facts.

        **Your agent's contents are auto-loaded into your context on boot — read them, don't re-fetch.**

        ## When to use
        - The moment the user tells you something worth keeping → `set` it on `mem`.
        - When durable state changes → `set` it on `data`.
        - Reading: it's already injected; only `read` / `keys` when you need a key not in context.

        ## Verbs
        - `read {key?}` — value at `key` (whole doc if omitted).
        - `keys {}` — list keys + sizes (the table-of-contents).
        - `set {key, value}` — upsert one key.
        - `delete {key}` — prune a key.
        - `replace {doc}` — overwrite the whole store (`{}` clears).
        - `state_yaml {}` — the whole store as YAML (the block injected on boot).

        ## Recipes
        - Remember a fact → `set {key:"user.name", value:"Ada"}`.
        - Save state → `set {key:"view.zoom", value:1.5}`.
        - Reuse keys, don't duplicate → `keys` first; use descriptive namespaced keys
          (`domain.subject.attribute`).
        - Self-contained values (the fact AND its why) →
          `set {key:"decision.db", value:"postgres — chosen over mysql for JSON support, 2026-05"}`.
        - Prune → `delete {key}` or `replace {doc}`.
        """
}
