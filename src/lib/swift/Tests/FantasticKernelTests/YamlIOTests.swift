// The on-disk YAML format — pinned byte-for-byte by the PYTHON-written fixture.
//
// Twin of python `kernel_state/tests/test_yaml_format.py`. The fixture
// `fixtures/agent.yaml` (a record) is what python's `yaml_io` writes for the
// input below; swift must (1) build the same input in code and write the same
// bytes, and (2) load the fixture and write it back byte-identical (load → dump
// is the identity on a python file). There is no message tape any more: the
// kernel keeps no conversation, and its execution rows live in its memory store.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private func obj(_ pairs: KeyValuePairs<String, JSON>) -> OrderedDictionary<String, JSON> {
    var r: OrderedDictionary<String, JSON> = [:]
    for (k, v) in pairs { r[k] = v }
    return r
}

/// `src/lib/python/bundled_agents/loader/kernel_state/tests/fixtures/<name>`.
private func fixture(_ name: String) throws -> String {
    let swiftDir = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    let url = swiftDir.deletingLastPathComponent()
        .appendingPathComponent("python/bundled_agents/loader/kernel_state/tests/fixtures")
        .appendingPathComponent(name)
    return try String(contentsOf: url, encoding: .utf8)
}

@Suite("agent.yaml format")
struct YamlIOTests {
    static let record: OrderedDictionary<String, JSON> = obj([
        "id": .string("tricky_0a1b2c"),
        "handler_module": .string("file_bridge.tools"),
        "parent_id": .string("kernel_state"),
        "one": .string("1"),
        "truthy": .string("true"),
        "nully": .string("null"),
        "tilde": .string("~"),
        "empty": .string(""),
        "yes": .string("yes"),
        "on": .string("on"),
        "octal12": .string("0o17"),
        "exp": .string("1e3"),
        "exp_neg": .string("2E-4"),
        "dot_end": .string("5."),
        "version": .string("1.2.3"),
        "date": .string("2026-09-25"),
        "colon": .string("a: b"),
        "hash": .string("# not a comment"),
        "lead": .string(" leading space"),
        "star": .string("*"),
        "unicode": .string("héllo — 世界"),
        "emoji": .string("ok 😀"),
        "int": .integer(42),
        "neg": .integer(-7),
        "float": .double(1.5),
        "bool": .bool(true),
        "none": .null,
        "multiline": .string("line one\nline two\n"),
        "no_final_nl": .string("a\n  indented b"),
        "trailing_blank": .string("keep\n\n"),
        "space_before_break": .string("trail \nx"),
        "tab": .string("a\tb"),
        "long": .string(String(repeating: "word ", count: 30)),
        "can_call": .object(obj([
            "$self": .string("*"), "$parent": .string("*"),
            "kernel_store": .array([.string("read"), .string("write")]),
        ])),
        "callable_by": .object(obj(["$parent": .string("*"), "$children": .string("*")])),
        "nested": .object(obj([
            "list": .array([.integer(1), .string("two"), .object(obj(["three": .array([])]))]),
            "map": .object([:]),
            "deep": .object(obj(["x": .array([.array([])])])),
        ])),
    ])

    @Test func theRecordMatchesThePythonFixtureBytes() throws {
        #expect(YamlIO.dump(.object(Self.record)) == (try fixture("agent.yaml")))
    }

    @Test func loadThenDumpIsTheIdentityOnThePythonFixture() throws {
        let text = try fixture("agent.yaml")
        #expect(YamlIO.dump(try YamlIO.load(text)) == text)
    }

    /// Byte-stable AND type-stable, key order kept (no sort), no `...` marker —
    /// through the record file primitives (python `write_record` / `read_tree`).
    @Test func aRecordRoundTripsTypeStableAndByteStable() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("fantastic-yaml-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appendingPathComponent(RECORD_FILE)
        try Persistence.recordBytes(Self.record).write(to: file)
        let first = try String(contentsOf: file, encoding: .utf8)
        let back = try #require(Persistence.readRecord(file))
        #expect(back == Self.record)  // JSON equality is type-strict (1 ≠ 1.0 ≠ "1")
        #expect(Array(back.keys) == Array(Self.record.keys))
        try Persistence.recordBytes(back).write(to: file)
        #expect(try String(contentsOf: file, encoding: .utf8) == first)
        #expect(!first.split(separator: "\n").contains("..."))
        let tree = Persistence.readTree(rootDir: dir)
        #expect(tree.first?.id == "tricky_0a1b2c")
    }

    /// A YAML STREAM (the scheduler's `schedule_events.yaml`): one `---`
    /// document per entry, every shape round-tripped and typed.
    @Test func aStreamOfDocumentsRoundTripsEveryShape() throws {
        let rows: [Row] = [
            obj([
                "id": .integer(1), "agent": .string("ai"), "parent": .null, "ts": .double(1.5),
                "content": .string("---\nnot a separator\n---\n"),
                "title": .string("quoted \"title\" with <tag> & 'x'"),
            ]),
            obj(["id": .integer(3), "refs": .array([.integer(1), .integer(2)])]),
            obj([
                "id": .integer(7),
                "payload": .object(obj([
                    "callable_by": .object(obj([
                        "$parent": .string("*"), "7": .integer(3), "a b": .null,
                    ])),
                    "bytes": .object(obj(["len": .integer(3)])),
                ])),
            ]),
        ]
        let text = rows.map(YamlIO.rowToDoc).joined()
        let back = try YamlIO.loadAll(text).compactMap { $0.asObject }
        #expect(back == rows)
    }

    /// The emitter's other shapes, pinned against what libyaml writes for them.
    @Test func edgeShapes() {
        #expect(YamlIO.dump(.array([])) == "[]\n")
        #expect(YamlIO.dump(.object([:])) == "{}\n")
        #expect(YamlIO.dump(.object(obj(["pid": .integer(123)]))) == "pid: 123\n")
        #expect(
            YamlIO.dump(.array([.object(obj(["a": .integer(1), "b": .array([.integer(2)])]))]))
                == "- a: 1\n  b:\n  - 2\n")
        #expect(YamlIO.dump(.object(obj(["k": .array([.array([.integer(1)])])]))) == "k:\n- - 1\n")
        #expect(YamlIO.dump(.object(obj(["f": .double(1e17), "g": .double(1e-5), "i": .double(.infinity)])))
                == "f: 1.0e+17\ng: 1.0e-05\ni: .inf\n")
        #expect(YamlIO.dump(.object(obj(["": .string("x"), "it's": .string("'q'")])))
                == "'': x\nit's: '''q'''\n")
    }
}
