// Stable identifier for an agent in the kernel tree.
//
// Mirrors `fantastic_kernel::AgentId` — a transparent newtype around
// String. Hashable for use as dictionary keys (the kernel's agents
// + inboxes maps are keyed by AgentId). Codable with a single-value
// container so on-disk JSON matches Rust's `#[serde(transparent)]`.

import Foundation

public struct AgentId: Sendable, Hashable, CustomStringConvertible {
    public let value: String

    public init(_ value: String) {
        self.value = value
    }

    public var description: String { value }
    public var asString: String { value }
}

extension AgentId: ExpressibleByStringLiteral {
    public init(stringLiteral value: String) {
        self.value = value
    }
}

extension AgentId: Codable {
    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        self.value = try container.decode(String.self)
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(value)
    }
}
