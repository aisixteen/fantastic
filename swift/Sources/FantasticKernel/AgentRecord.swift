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

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: AnyKey.self)
        try container.encode(id, forKey: AnyKey(stringValue: "id"))
        if let handlerModule {
            try container.encode(handlerModule, forKey: AnyKey(stringValue: "handler_module"))
        }
        if let parentId {
            try container.encode(parentId, forKey: AnyKey(stringValue: "parent_id"))
        }
        for (k, v) in meta where !AgentRecord.reservedKeys.contains(k) {
            try container.encode(v, forKey: AnyKey(stringValue: k))
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
