// AgentRecord Codable tests — most importantly, the `meta` flatten.

import FantasticJSON
import OrderedCollections
import Testing

@testable import FantasticKernel

@Suite("AgentRecord Codable")
struct AgentRecordCodableTests {
    @Test func decodesMinimalRecord() throws {
        let json = #"{"id":"alpha"}"#
        let rec = try decode(json)
        #expect(rec.id == "alpha")
        #expect(rec.handlerModule == nil)
        #expect(rec.parentId == nil)
        #expect(rec.meta.isEmpty)
    }

    @Test func decodesFullRecord() throws {
        let json = #"""
        {"id":"web_1","handler_module":"web.tools","parent_id":"core","port":31415}
        """#
        let rec = try decode(json)
        #expect(rec.id == "web_1")
        #expect(rec.handlerModule == "web.tools")
        #expect(rec.parentId == "core")
        #expect(rec.meta["port"]?.asInt == 31415)
    }

    @Test func decodeFlattenedMetaCapturesUnknownKeys() throws {
        // Anything that isn't a kernel-managed field lands in `meta`.
        let json = #"""
        {"id":"x","display_name":"Demo","color":"#ff0099","options":{"k":1}}
        """#
        let rec = try decode(json)
        #expect(rec.meta["display_name"]?.asString == "Demo")
        #expect(rec.meta["color"]?.asString == "#ff0099")
        #expect(rec.meta["options"]?["k"].asInt == 1)
        // Reserved keys NOT in meta.
        #expect(rec.meta["id"] == nil)
    }

    @Test func decodeMissingIdThrows() {
        let json = #"{"handler_module":"x"}"#
        #expect(throws: DecodingError.self) {
            _ = try decode(json)
        }
    }

    @Test func encodeRoundTripPreservesMeta() throws {
        var meta: OrderedDictionary<String, JSON> = [:]
        meta["display_name"] = .string("My Agent")
        meta["port"] = .integer(8080)

        let rec = AgentRecord(
            id: "demo",
            handlerModule: "file.tools",
            parentId: "core",
            meta: meta
        )
        let data = try JSONEncoder().encode(rec)
        let decoded = try JSONDecoder().decode(AgentRecord.self, from: data)
        #expect(decoded.id == "demo")
        #expect(decoded.handlerModule == "file.tools")
        #expect(decoded.parentId == "core")
        #expect(decoded.meta["display_name"]?.asString == "My Agent")
        #expect(decoded.meta["port"]?.asInt == 8080)
    }

    @Test func encodeDoesNotEmitNilOptionals() throws {
        let rec = AgentRecord(id: "minimal")
        let data = try JSONEncoder().encode(rec)
        let str = String(data: data, encoding: .utf8) ?? ""
        #expect(!str.contains("handler_module"))
        #expect(!str.contains("parent_id"))
    }

    @Test func encodeReservedKeysInMetaAreSkipped() throws {
        // If someone smuggles a reserved key into meta, it must NOT
        // overwrite the top-level field on encode. Defensive — the
        // Rust kernel guards this same way via its persistence layer.
        var meta: OrderedDictionary<String, JSON> = [:]
        meta["id"] = .string("WRONG")
        meta["handler_module"] = .string("WRONG.tools")
        let rec = AgentRecord(
            id: "real_id",
            handlerModule: "real.tools",
            meta: meta
        )
        let data = try JSONEncoder().encode(rec)
        let parsed = try JSON.parse(data)
        #expect(parsed["id"].asString == "real_id")
        #expect(parsed["handler_module"].asString == "real.tools")
    }

    // MARK: helper

    private func decode(_ string: String) throws -> AgentRecord {
        let data = string.data(using: .utf8)!
        return try JSONDecoder().decode(AgentRecord.self, from: data)
    }
}

import Foundation
