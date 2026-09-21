// On-disk shape of a single agent.
//
// Mirrors `fantastic_kernel::AgentRecord` — the per-agent
// `agent.json` file in `.fantastic/agents/<id>/`. Kernel-managed
// fields (`id`, `handler_module`, `parent_id`) live at the top
// level; everything else (display_name, port, custom bundle config)
// is flattened into `meta` so the same JSON round-trips between
// Rust + Swift without any field reshape.
//
// `#[serde(flatten)]` in Rust has no direct Codable equivalent —
// we walk a dynamic-keyed container manually to capture the
// flattened tail.

import FantasticJSON
import Foundation
import OrderedCollections

public struct AgentRecord: Sendable, Equatable {
    public var id: String
    public var handlerModule: String?
    public var parentId: String?
    public var meta: OrderedDictionary<String, JSON>

    public init(
        id: String,
        handlerModule: String? = nil,
        parentId: String? = nil,
        meta: OrderedDictionary<String, JSON> = [:]
    ) {
        self.id = id
        self.handlerModule = handlerModule
        self.parentId = parentId
        self.meta = meta
    }
}

extension AgentRecord: Codable {
    /// Reserved top-level keys — every other key gets routed into `meta`.
    /// Matches the Rust `AgentRecord` field names verbatim (snake_case).
    private static let reservedKeys: Set<String> = [
        "id", "handler_module", "parent_id",
    ]

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: AnyKey.self)
        var id: String?
        var handlerModule: String?
        var parentId: String?
        var meta: OrderedDictionary<String, JSON> = [:]

        for key in container.allKeys {
            switch key.stringValue {
            case "id":
                id = try container.decode(String.self, forKey: key)
            case "handler_module":
                handlerModule = try container.decodeIfPresent(String.self, forKey: key)
            case "parent_id":
                parentId = try container.decodeIfPresent(String.self, forKey: key)
            default:
                meta[key.stringValue] = try container.decode(JSON.self, forKey: key)
            }
        }

        guard let id = id else {
            throw DecodingError.keyNotFound(
                AnyKey(stringValue: "id"),
                DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "AgentRecord: missing required field `id`"
                ))
        }
        self.id = id
        self.handlerModule = handlerModule
        self.parentId = parentId
        self.meta = meta
    }

    /// `parent_id` is declared AFTER the flattened meta — python's `record`
    /// writes `id`, `handler_module`, the meta, and `parent_id` LAST, so a
    /// `parent_id` that leaked into meta cannot shadow the tree's answer.
    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: AnyKey.self)
        try container.encode(id, forKey: AnyKey(stringValue: "id"))
        if let handlerModule {
            try container.encode(handlerModule, forKey: AnyKey(stringValue: "handler_module"))
        }
        for (k, v) in meta where !AgentRecord.reservedKeys.contains(k) {
            try container.encode(v, forKey: AnyKey(stringValue: k))
        }
        if let parentId {
            try container.encode(parentId, forKey: AnyKey(stringValue: "parent_id"))
        }
    }

    // Dynamic-key container helper (local copy since the one in
    // FantasticJSON is internal to that module).
    private struct AnyKey: CodingKey {
        var stringValue: String
        var intValue: Int? { nil }
        init(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { nil }
    }
}

extension AgentRecord {
    /// The record as ORDERED json — python `Agent.record`:
    ///
    ///     rec = {"id": self.id}
    ///     if handler_module is not None: rec["handler_module"] = ...
    ///     rec.update(self._meta)
    ///     rec["parent_id"] = parent.id      # or popped, for the root
    ///
    /// so the keys come out `id, handler_module?, <meta in order>, parent_id`.
    /// Never route this through `JSONEncoder`: Foundation reorders a keyed
    /// container per process, which scrambled every `list_agents` row, every
    /// `create_agent`/`update_agent` reply and every nested grant map.
    public var ordered: OrderedDictionary<String, JSON> {
        var rec: OrderedDictionary<String, JSON> = ["id": .string(id)]
        if let handlerModule { rec["handler_module"] = .string(handlerModule) }
        // `dict.update` semantics: a meta key that shadows a structural one
        // overwrites IN PLACE, keeping the position it already had.
        for (k, v) in meta { rec[k] = v }
        if let parentId {
            rec["parent_id"] = .string(parentId)
        } else {
            // The root has no parent, whatever meta says.
            rec.removeValue(forKey: "parent_id")
        }
        return rec
    }

    /// `ordered` as a JSON object.
    public var json: JSON { .object(ordered) }

    /// Build a record from an on-disk `agent.json` object PRESERVING ITS KEY
    /// ORDER. `Codable`'s keyed container hands its keys over in hash order, so
    /// hydrating through it scrambled every record's meta (and every nested
    /// grant map) on each boot. `JSON.parse` keeps the file's own order.
    public init(json: JSON) throws {
        guard case .object(let obj) = json, let id = obj["id"]?.asString else {
            throw KernelError.invalidPayload("agent.json: missing required field `id`")
        }
        var meta: OrderedDictionary<String, JSON> = [:]
        for (k, v) in obj where !AgentRecord.reservedKeys.contains(k) { meta[k] = v }
        self.init(
            id: id,
            handlerModule: obj["handler_module"]?.asString,
            parentId: obj["parent_id"]?.asString,
            meta: meta)
    }
}

/// Keys a RECORD may never carry: they name substrate wiring, not agent meta.
///
/// A hand-authored `root_path:` once cleared the whole tree and THEN raised —
/// the kernel was bricked by a field that looked like configuration. Refused at
/// the door, before any teardown, so a bad document cannot half-apply.
public let reservedRecordKeys: Set<String> = ["root_path", "ctx", "parent"]
