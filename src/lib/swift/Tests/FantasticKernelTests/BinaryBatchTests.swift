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
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON?, Data?) {
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
                binary: payload)
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
                id: "job-b", binary: first)
        ])[0]
        #expect(out.result["written"].asInt == 3, "\(out.result.serialize())")
        // A NAMED item is ledgered under the int its name opened — the pid IS that id.
        #expect(out.pid?.value == kernel.ledger.resolveName("job-b"))

        // The SAME id re-sent returns the recorded outcome and runs nothing.
        let again = await kernel.send([
            Item(
                AgentId("sink"),
                .object(["type": .string("write_stream"), "path": .string("a")]),
                id: "job-b", binary: Data([9, 9, 9, 9, 9, 9]))
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
                timeout: 5.0, binary: payload)
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
                binary: Data("x".utf8))
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
                binary: Data("x".utf8))
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
                agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
            ) async throws -> (JSON?, Data?) {
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
                binary: Data("x".utf8))
        ])[0]
        #expect(out.status == .failed, "\(out.result.serialize())")
        #expect(out.result["raised"].asBool == true, "\(out.result.serialize())")
    }
}
