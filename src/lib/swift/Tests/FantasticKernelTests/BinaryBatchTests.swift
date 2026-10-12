// A binary item rides the ORDINARY batch.
//
// `sendWithBinary` bypassed `send` entirely, so a `write_stream` the model
// issued through `send_binary` had no task row (invisible to `ps`, unreachable
// by `cancel`), no ledger id (no dedup, no join) and no timeout — and the reply
// tag it produced read `[reply to - · …]` because there was no pid to name
// (measured 2026-09-02). An `Item` may now carry the blob, and the kernel
// dispatches it through `handleBinary` under the same row, id and deadline as
// any other item.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

/// Records what reached the BINARY channel, and answers the text channel with a
/// refusal so a mis-routed call cannot look like success.
private final class Sink: AgentBundle, @unchecked Sendable {
    let name = "sink"
    private let lock = NSLock()
    private var seen = Data()
    var received: Data { lock.withLock { seen } }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["error": .string("text channel — the blob did not ride")])
    }
    func handleBinary(
        agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
    ) async throws -> (JSON?, Blob?) {
        // the chunk is the payload's `bytes` — python's `payload.get("bytes", b"")`
        let blob = bodyAt(body, "bytes") ?? Data()
        lock.withLock { seen = blob }
        return (.object(["written": .integer(Int64(blob.count))]), nil)
    }
}

@Suite("binary batch")
struct BinaryBatchTests {

    private func mk() async -> (Kernel, Sink) {
        let sink = Sink()
        let registry = BundleRegistry()
        registry.register("sink.tools", sink)
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "id": .string("sink"), "handler_module": .string("sink.tools"),
                ]))
        ])[0].result
        return (kernel, sink)
    }

    @Test func aBinaryItemReachesTheBinaryChannelWithARealPid() async {
        let (kernel, sink) = await mk()
        // Non-utf8 bytes: proves the blob rides raw, never through JSON.
        let payload = Data((0...255).map { UInt8($0) })
        let out = await kernel.send([
            Item(
                AgentId("sink"),
                .object(["type": .string("write_stream"), "path": .string("blob.bin")]),
                binary: Blob("bytes", payload))
        ])[0]
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result["written"].asInt == Int64(payload.count), "\(out.result.serialize())")
        #expect(sink.received == payload, "bytes must ride verbatim")
        #expect((out.pid?.value ?? 0) > 0, "a pid is an int from the one counter: \(String(describing: out.pid))")
    }

    @Test func aNamedBinaryItemDedupsLikeAnyOther() async {
        let (kernel, sink) = await mk()
        let first = Data([1, 2, 3])
        let out = await kernel.send([
            Item(
                AgentId("sink"),
                .object(["type": .string("write_stream"), "path": .string("a")]),
                id: "job-b", binary: Blob("bytes", first))
        ])[0]
        #expect(out.result["written"].asInt == 3, "\(out.result.serialize())")
        // A NAMED item is ledgered under the int its name opened — the pid IS that id.
        #expect(out.pid?.value == kernel.ledger.resolveName("job-b"))

        // The SAME id re-sent returns the recorded outcome and runs nothing.
        let again = await kernel.send([
            Item(
                AgentId("sink"),
                .object(["type": .string("write_stream"), "path": .string("a")]),
                id: "job-b", binary: Blob("bytes", Data([9, 9, 9, 9, 9, 9])))
        ])[0]
        #expect(again.result["written"].asInt == 3, "the recorded outcome, not a rerun")
        #expect(sink.received == first, "the handler must not have run a second time")
    }

    @Test func aTimedBinaryItemStillReachesTheBinaryChannel() async {
        // The item's own DEADLINE must not cost it its bytes. `runDispatchOutcome`
        // had two branches and only the untimed one forwarded the blob, so every
        // item carrying a timeout silently fell to the TEXT channel. AI routing
        // stamps the layer's deadline on EVERY item, which made this the ONLY
        // shape a model could ever produce (e2e `E10_dialog:swift`, 2026-09-03).
        let (kernel, sink) = await mk()
        let payload = Data("42\n".utf8)
        let out = await kernel.send([
            Item(
                AgentId("sink"),
                .object(["type": .string("write_stream"), "path": .string("answer")]),
                timeout: 5.0, binary: Blob("bytes", payload))
        ])[0]
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result["written"].asInt == Int64(payload.count), "\(out.result.serialize())")
        #expect(sink.received == payload, "a deadline must not divert the blob")
    }

    /// C34: `send_binary` to a BARE agent (no handler_module) answers the SAME
    /// UNHANDLED text + `verb` the text channel gives — python has no separate
    /// binary dispatch at all, one `_dispatch` resolution serves both.
    @Test func binaryToABareAgentIsTheSameUnhandledTextAsTheTextChannel() async {
        let registry = BundleRegistry()
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let textReply = await kernel.send([
            Item(AgentId("kernel_state"), .object(["type": .string("write_stream")]))
        ])[0].result
        let binaryReply = await kernel.send([
            Item(
                AgentId("kernel_state"), .object(["type": .string("write_stream")]),
                binary: Blob("bytes", Data("x".utf8)))
        ])[0].result
        #expect(binaryReply["error"] == textReply["error"], "\(binaryReply) vs \(textReply)")
        #expect(binaryReply["reason"].asString == "unhandled")
        #expect(binaryReply["verb"].asString == "write_stream")
    }

    /// C34/C24: `send_binary` naming a `handler_module` no bundle answers
    /// answers the SAME `not_installed` text + `verb` as the text channel.
    @Test func binaryToAnUninstalledHandlerModuleIsTheSameTextAsTheTextChannel() async {
        let registry = BundleRegistry()
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let ghost = Agent(
            id: AgentId("ghost"), handlerModule: "missing.tools", parentId: root.id)
        kernel.register(ghost)
        root.insertChild(ghost)
        let textReply = await kernel.send([
            Item(AgentId("ghost"), .object(["type": .string("write_stream")]))
        ])[0].result
        let binaryReply = await kernel.send([
            Item(
                AgentId("ghost"), .object(["type": .string("write_stream")]),
                binary: Blob("bytes", Data("x".utf8)))
        ])[0].result
        #expect(binaryReply["error"] == textReply["error"], "\(binaryReply) vs \(textReply)")
        #expect(
            binaryReply["error"].asString?.contains("missing.tools") == true, "\(binaryReply)")
        #expect(binaryReply["reason"].asString == "unhandled")
        #expect(binaryReply["verb"].asString == "write_stream")
    }

    /// C34: a binary handler that THROWS settles `raised:true`, exactly like
    /// the text channel's catch — a handler blowing up is a RESULT, not a
    /// crash, either way.
    @Test func aRaisingBinaryHandlerSettlesRaisedTrue() async {
        struct Boom: Error {}
        struct Raiser: AgentBundle {
            let name = "raiser"
            func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
                nil
            }
            func handleBinary(
                agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
            ) async throws -> (JSON?, Blob?) {
                throw Boom()
            }
        }
        let registry = BundleRegistry()
        registry.register("raiser.tools", Raiser())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "id": .string("raiser"), "handler_module": .string("raiser.tools"),
                ]))
        ])
        let out = await kernel.send([
            Item(
                AgentId("raiser"), .object(["type": .string("write_stream")]),
                binary: Blob("bytes", Data("x".utf8)))
        ])[0]
        #expect(out.status == .failed, "\(out.result.serialize())")
        #expect(out.result["raised"].asBool == true, "\(out.result.serialize())")
    }

    /// A RAW BODY rides at ITS payload path — where python's dict holds the bytes
    /// (`data` for `paste_image`, `bytes` for `write_stream`). The handler gets it
    /// at that path and a header with NOTHING there; a watcher gets it at that
    /// path with `null` in python's position; the call row records `{len}` there.
    /// A path that names no slot of the payload is refused before anything runs.
    /// Rust `a_binary_body_rides_at_its_payload_path`.
    @Test func aBinaryBodyRidesAtItsPayloadPath() async throws {
        struct Echo: AgentBundle {
            let name = "echo"
            func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
                .object(["channel": "text"])
            }
            func handleBinary(
                agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
            ) async throws -> (JSON?, Blob?) {
                let data = bodyAt(body, "data").map { JSON.integer(Int64($0.count)) } ?? .null
                let bytes = bodyAt(body, "bytes").map { JSON.integer(Int64($0.count)) } ?? .null
                return (.object(["header": header, "data": data, "bytes": bytes]), nil)
            }
        }
        let registry = BundleRegistry()
        registry.register("echo.tools", Echo())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": "create_agent", "parent_id": "kernel_state", "id": "echo",
                    "handler_module": "echo.tools",
                ]))
        ])
        let tap = kernel.ensureInbox(AgentId("tap"))
        #expect(kernel.watch(src: AgentId("echo"), watcher: AgentId("tap")) == nil)

        // the caller names the slot (`data: null`) to keep python's key order
        let out = await kernel.send([
            Item(
                AgentId("echo"),
                .object(["type": "paste_image", "job_id": 1, "data": .null, "mime": "image/png"]),
                binary: Blob("data", Data([9, 8, 7])))
        ])[0]
        #expect(
            out.result
                == .object([
                    "header": .object(["type": "paste_image", "job_id": 1, "mime": "image/png"]),
                    "data": 3, "bytes": .null,
                ]),
            "the handler reads its body at `data`, nothing there in its header: \(out.result.serialize())")
        let got = try #require(tap.tryTake(), "the watcher was mirrored")
        #expect(
            got.payload.serialize()
                == #"{"type":"paste_image","job_id":1,"data":null,"mime":"image/png"}"#)
        #expect(got.body?.parts == ["data"] && got.body?.bytes == Data([9, 8, 7]))
        let row = try #require(
            kernel.rows.all().first { $0["verb"]?.asString == "paste_image" }, "the call row")
        #expect(
            (row["payload"] ?? .null).serialize()
                == #"{"type":"paste_image","job_id":1,"data":{"len":3},"mime":"image/png"}"#)

        // a NESTED path: the slot inside the payload, the leaf gone from the header
        let nested = await kernel.send([
            Item(
                AgentId("echo"), .object(["type": "take", "meta": ["k": 1]]),
                binary: Blob(parts: ["meta", "blob"], Data([1])))
        ])[0]
        #expect(nested.result["header"] == .object(["type": "take", "meta": ["k": 1]]))
        #expect(nested.result["data"] == .null)
        let mirrored = try #require(tap.tryTake(), "mirrored")
        #expect(mirrored.payload["meta"] == .object(["k": 1, "blob": .null]))
        #expect(mirrored.body?.parts == ["meta", "blob"] && mirrored.body?.bytes == Data([1]))

        // ONE LITERAL KEY with a dot in it (web_rest's `X-Fantastic-Bytes-Field:
        // a.b`, python `payload["a.b"] = body`): the slot is the key "a.b", and the
        // nested `a.b` beside it is another slot, left as sent — in the handler's
        // header, the watcher's frame and the call row alike
        let literal = await kernel.send([
            Item(
                AgentId("echo"), .object(["type": "take", "a": ["b": "kept"]]),
                binary: Blob(parts: ["a.b"], Data([4, 5, 6])))
        ])[0]
        #expect(
            literal.result["header"] == .object(["type": "take", "a": ["b": "kept"]]),
            "\(literal.result.serialize())")
        let lit = try #require(tap.tryTake(), "mirrored")
        #expect(lit.payload.serialize() == #"{"type":"take","a":{"b":"kept"},"a.b":null}"#)
        #expect(lit.body?.parts == ["a.b"] && lit.body?.bytes == Data([4, 5, 6]))
        let litRow = try #require(
            kernel.rows.all().last { $0["verb"]?.asString == "take" }, "the call row")
        #expect(
            (litRow["payload"] ?? .null).serialize()
                == #"{"type":"take","a":{"b":"kept"},"a.b":{"len":3}}"#)

        // a list slot, by INDEX: the body takes the list's element in place
        let listed = await kernel.send([
            Item(
                AgentId("echo"), .object(["type": "take", "items": [["n": 0], ["data": .null]]]),
                binary: Blob(parts: ["items", 1, "data"], Data([7])))
        ])[0]
        #expect(listed.result["data"] == .null, "\(listed.result.serialize())")
        let lst = try #require(tap.tryTake(), "mirrored")
        #expect(lst.payload.serialize() == #"{"type":"take","items":[{"n":0},{"data":null}]}"#)
        #expect(lst.body?.parts == ["items", 1, "data"] && lst.body?.bytes == Data([7]))

        // a path that names no slot of the payload is refused, nothing runs; the
        // refusal names the steps as `_binary_path` spells them
        let refused = await kernel.send([
            Item(
                AgentId("echo"), .object(["type": "take"]),
                binary: Blob(parts: ["nope", "deep"], Data([1])))
        ])[0]
        #expect(refused.pid == nil)
        #expect(refused.result["reason"] == "bad_arg", "\(refused.result.serialize())")
        #expect(refused.result["arg"] == "binary")
        #expect(
            refused.result["error"].asString?.hasSuffix("got ['nope', 'deep']") == true,
            "\(refused.result.serialize())")
        // a key where the payload holds a LIST is no slot either: a list step is
        // an index, never a digit string
        let digit = await kernel.send([
            Item(
                AgentId("echo"), .object(["type": "take", "items": [1, 2]]),
                binary: Blob(parts: ["items", "0"], Data([1])))
        ])[0]
        #expect(digit.pid == nil && digit.result["arg"] == "binary", "\(digit.result.serialize())")
        #expect(tap.tryTake() == nil, "a refused item mirrors nothing")
    }

    /// A RAW READ rides the binary channel with NO request body: the handler gets
    /// `nil`, the call row and the watcher carry the plain payload (python's
    /// `read_stream` dict holds no bytes), and the reply's chunk comes back raw.
    @Test func aRawReadCarriesNoBodyAndRecordsAPlainPayload() async throws {
        struct Chunk: AgentBundle {
            let name = "chunk"
            func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
                .object(["channel": "text"])
            }
            func handleBinary(
                agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
            ) async throws -> (JSON?, Blob?) {
                (.object(["had_body": .bool(body != nil), "eof": true]), Blob("bytes", Data([4, 2])))
            }
        }
        let registry = BundleRegistry()
        registry.register("chunk.tools", Chunk())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": "create_agent", "parent_id": "kernel_state", "id": "chunk",
                    "handler_module": "chunk.tools",
                ]))
        ])
        let tap = kernel.ensureInbox(AgentId("tap"))
        #expect(kernel.watch(src: AgentId("chunk"), watcher: AgentId("tap")) == nil)
        let out = await kernel.send([
            Item(AgentId("chunk"), .object(["type": "read_stream", "path": "x"]), rawReply: true)
        ])[0]
        #expect(out.result == .object(["had_body": false, "eof": true]))
        #expect(out.body == Blob("bytes", Data([4, 2])))
        let got = try #require(tap.tryTake())
        #expect(got.body == nil && got.payload.serialize() == #"{"type":"read_stream","path":"x"}"#)
        let row = try #require(
            kernel.rows.all().first { $0["verb"]?.asString == "read_stream" }, "the call row")
        #expect((row["payload"] ?? .null).serialize() == #"{"type":"read_stream","path":"x"}"#)
    }
}
