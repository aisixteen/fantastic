// `_binary_path` is a LIST OF STEPS (owner-approved, 2026-10-02).
//
// A binary frame's header names where its raw body belongs. It used to spell that
// as a dotted string, so a payload whose bytes sat at the LITERAL key `"a.b"`
// (which web_rest's `X-Fantastic-Bytes-Field: a.b` binds) went out as
// `"payload.a.b"` and the receiver rebuilt a NESTED `a` → `b` slot. Now the path
// is a JSON array: a string step is an object key, taken literally; an integer
// step is a list index.
//
// The codec half of python `io_bridge/tests/test_binary_path_steps.py` (a): literal,
// nested and list paths round-trip; every malformed path off the wire is one
// `FrameError` carrying python's exact text. The legs are pinned beside their own
// suites: `BinaryBodyPathTests` (web_ws + a ws_bridge forward), `RelayTransportTests`
// (relay_connector), `PeerCallsTests` (web_rest's binding to a peer kernel) and
// `ExecBridgeTests` (the guest wire).

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import Testing

/// A binary frame: `head` (already carrying `_binary_path`) + `body`.
private func frame(_ head: JSON, _ body: Data = Data("zz".utf8)) -> Data {
    Codec.encodeBinaryFrame(header: head, body: body)
}

/// The header text the frame carries, verbatim.
private func headText(_ wire: Data) -> String {
    let n = wire.prefix(4).reduce(0) { Int($0) << 8 | Int($1) }
    return String(decoding: wire.dropFirst(4).prefix(n), as: UTF8.self)
}

@Suite("binary path — a list of steps")
struct BinaryPathStepsTests {

    @Test func aLiteralDottedKeyRoundTripsBesideItsNestedTwin() throws {
        let head: JSON = [
            "type": "call", "id": "1", "target": "echo",
            "payload": ["type": "take", "a": ["b": "kept"], "a.b": .null],
            "_binary_path": Codec.payloadPath(["a.b"]),
        ]
        let wire = frame(head, Data([0, 0x72, 0xFF]))
        // python's `json.dumps` of the same header, byte for byte
        #expect(
            headText(wire)
                == #"{"type": "call", "id": "1", "target": "echo", "payload": {"type": "take", "a": {"b": "kept"}, "a.b": null}, "_binary_path": ["payload", "a.b"]}"#
        )
        let (h, body) = try Codec.decodeBinaryFrame(wire)
        var env = h
        let slot = try Codec.placeBody(&env)
        #expect(slot == ["payload", "a.b"])
        #expect(body == Data([0, 0x72, 0xFF]))
        // the null sits at the literal key; the nested twin is untouched
        #expect(
            env == [
                "type": "call", "id": "1", "target": "echo",
                "payload": ["type": "take", "a": ["b": "kept"], "a.b": .null],
            ])
        let (payload, binary) = Codec.callPayload(h, body: body)
        #expect(payload == ["type": "take", "a": ["b": "kept"], "a.b": .null])
        #expect(binary == Blob(parts: ["a.b"], body))
    }

    /// Nested, list and odd-key paths: what the header names is what the receiver
    /// walks — a key that looks like an index stays a string key.
    @Test(
        arguments: [
            (["data": ["bytes": .null]], ["data", "bytes"]),
            (["data": ["items": [["n": 1], ["data": .null]]]], ["data", "items", 1, "data"]),
            (["payload": [.null, "tail"]], ["payload", 0]),
            (["payload": ["0": .null]], ["payload", "0"]),
            (["payload": ["": .null]], ["payload", ""]),
            (["payload": ["a..b.": ["c.": .null]]], ["payload", "a..b.", "c."]),
        ] as [(JSON, [PathStep])])
    func nestedListAndOddKeyPathsRoundTrip(_ env: JSON, _ path: [PathStep]) throws {
        var head = env
        head["_binary_path"] = Codec.binaryPath(path)
        let (h, body) = try Codec.decodeBinaryFrame(frame(head, Data([1, 2])))
        #expect(h["_binary_path"] == Codec.binaryPath(path))
        var placed = h
        #expect(try Codec.placeBody(&placed) == path)
        #expect(placed == env, "\(placed.serialize())")
        #expect(body == Data([1, 2]))
    }

    @Test func aPathWithNoSlotOrNoPathPlacesNothing() throws {
        // absent or null ⇒ no body is placed, exactly as before
        for head: JSON in [["type": "x"], ["type": "x", "_binary_path": .null]] {
            var env = try Codec.decodeBinaryFrame(frame(head)).header
            #expect(try Codec.placeBody(&env) == nil)
            #expect(env == ["type": "x"])
        }
    }

    /// Every path python's `set_path` cannot place is the frame's error, with
    /// python's text: `bad _binary_path <repr(path)>: <why>`.
    @Test(
        arguments: [
            (JSON.string("payload.bytes"), "not a list of steps"),  // the old dotted spelling
            ([], "an empty path names no slot"),
            (["payload": "bytes"], "not a list of steps"),
            ([0], "step 0 at an object is not a string key"),
            (["items", "0"], "step '0' at a list is not a non-negative integer index"),
            (["items", -1], "step -1 at a list is not a non-negative integer index"),
            (["items", .bool(true)], "step True at a list is not a non-negative integer index"),
            (["items", .double(0.0)], "step 0.0 at a list is not a non-negative integer index"),
            (["items", 2], "index 2 is past the end of a list of 2"),
            (["nope", "x"], "no key 'nope'"),
            (["type", "x"], "step 'x' goes into a string, not a container"),
            (["items", 0, "x"], "step 'x' goes into a null, not a container"),
            (["n", "x"], "step 'x' goes into a number, not a container"),
        ] as [(JSON, String)])
    func aMalformedPathOffTheWireIsOneFrameError(_ path: JSON, _ why: String) {
        let head: JSON = [
            "type": "reply", "items": [.null, .null], "n": 1, "_binary_path": path,
        ]
        #expect(throws: Codec.FrameError("bad _binary_path \(pyRepr(path)): \(why)")) {
            _ = try Codec.decodeBinaryFrame(frame(head))
        }
        var h = head
        #expect(throws: Codec.FrameError("bad _binary_path \(pyRepr(path)): \(why)")) {
            _ = try Codec.placeBody(&h)
        }
    }

    /// The python texts, spelled out once — `pyRepr` of each path above.
    @Test func theErrorTextNamesThePathAsPythonReprsIt() {
        let head: JSON = [
            "type": "reply", "items": [.null, .null], "_binary_path": ["items", .bool(true)],
        ]
        #expect(
            throws: Codec.FrameError(
                "bad _binary_path ['items', True]: step True at a list is not a non-negative integer index"
            )
        ) {
            _ = try Codec.decodeBinaryFrame(frame(head))
        }
        let dotted: JSON = ["type": "reply", "_binary_path": "payload.bytes"]
        #expect(throws: Codec.FrameError("bad _binary_path 'payload.bytes': not a list of steps")) {
            _ = try Codec.decodeBinaryFrame(frame(dotted))
        }
    }
}
