// `send` validates the ITEM before anything is resolved, gated or minted.
//
// Measured 2026-09-02 (python, ported): a payload without `type` reached the
// gate and the target instead of being a coded refusal; a non-string `type`
// raised out of `send`; rust/swift minted an id when the caller supplied `""`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private struct Noop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
}

@Suite("send validation")
struct SendValidationTests {

    private func freshKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("noop.tools", Noop())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    @discardableResult
    private func create(_ kernel: Kernel, _ id: String) async -> JSON {
        await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "id": .string(id), "handler_module": .string("noop.tools"),
                ]))
        ])[0].result
    }

    @Test func missingTypeIsMissingArgBeforeTheGate() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        await create(kernel, "b")  // no edge a→b: the gate WOULD refuse
        let out = await KernelTaskLocals.$currentSender.withValue(AgentId("a")) {
            await kernel.send([Item(AgentId("b"), .object([:]))])[0]
        }
        #expect(out.pid == nil, "\(out.result.serialize())")
        #expect(out.result["reason"].asString == "missing_arg", "\(out.result.serialize())")
        #expect(out.result["arg"].asString == "payload.type")
        #expect(out.result["side"] == .null, "validated BEFORE the gate — no gate side")
    }

    @Test func nonStringTypeIsBadArg() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        // A non-string is named by its TYPE, never echoed — python's
        // `type(verb).__name__` (audit 2026-10-03, W-11).
        let cases: [(JSON, String)] = [
            (.array([.string("x")]), "list"), (.integer(7), "int"), (.double(1.5), "float"),
            (.object(["a": .integer(1)]), "dict"), (.string(""), "''"), (.null, "NoneType"),
        ]
        for (bad, got) in cases {
            let out = await kernel.send([Item(AgentId("a"), .object(["type": bad]))])[0]
            #expect(out.result["reason"].asString == "bad_arg", "\(bad): \(out.result.serialize())")
            #expect(out.result["arg"].asString == "payload.type")
            #expect(
                out.result["error"].asString
                    == "send: payload.type must be a non-empty string, got \(got)")
        }
    }

    /// A RAW BODY sent AS `type` (the octet REST leg's `X-Fantastic-Bytes-Field:
    /// type`, a binary frame's `_binary_path: [payload, type]`) is python's
    /// `bytes` there: named by its type, never echoed — python's repr of it made
    /// a 1 MiB body a 5 MiB refusal (audit 2026-10-03, W-11).
    @Test func aBodySentAsTypeIsRefusedByItsType() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        for payload: JSON in [.object(["type": .null]), .object([:])] {
            var item = Item(AgentId("a"), payload)
            item.binary = Blob("type", Data(count: 4096))
            let out = await kernel.send([item])[0]
            #expect(
                out.result["error"].asString
                    == "send: payload.type must be a non-empty string, got bytes",
                "\(out.result.serialize())")
            #expect(out.result["reason"].asString == "bad_arg")
            #expect(out.result["arg"].asString == "payload.type")
        }
    }

    @Test func emptyIdIsRefusedBadArg() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        let item = Item(AgentId("a"), .object(["type": .string("poke")]), id: "")
        let out = await kernel.send([item])[0]
        #expect(out.pid == nil)
        #expect(out.result["reason"].asString == "bad_arg", "\(out.result.serialize())")
        #expect(out.result["arg"].asString == "id")
        // NO MINTED NAMESPACE. A caller's id is a NAME, any non-empty string;
        // ids are ints the kernel mints. The old `<agent>:<hex>` refusal was
        // swift's own and is gone (python parity, 2026-09-12).
    }

    @Test func createAgentWithEmptyIdIsRefused() async {
        let kernel = freshKernel()
        let before = kernel.allAgents().count
        let r = await create(kernel, "")
        #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
        #expect(r["arg"].asString == "id")
        #expect(kernel.allAgents().count == before, "no agent minted for \"\"")
    }

    /// C30: an empty (or absent, by construction — `AgentId` is never
    /// optional here, so an empty string is the only way to reach it) target
    /// is `missing_arg`, checked FIRST — before payload/timeout/verb.
    @Test func emptyTargetIsMissingArg() async {
        let kernel = freshKernel()
        let out = await kernel.send([Item(AgentId(""), .object(["type": .string("poke")]))])[0]
        #expect(out.pid == nil)
        #expect(out.result["reason"].asString == "missing_arg", "\(out.result.serialize())")
        #expect(out.result["arg"].asString == "target")
    }

    /// C30/C38: `id` type-checks with python's exact "got <repr>" tail; a
    /// bool is refused the same as any other non-str/non-int (python: bool
    /// is an int SUBCLASS, but this kernel's `JSON` keeps the cases distinct,
    /// so a raw `true`/`false` id still lands here, matching python's
    /// explicit `isinstance(x, bool)` carve-out).
    @Test func rawIdWrongShapeIsBadArgWithGotRepr() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        var item = Item(AgentId("a"), .object(["type": .string("poke")]))
        item.rawId = .bool(true)
        let out = await kernel.send([item])[0]
        #expect(out.result["reason"].asString == "bad_arg", "\(out.result.serialize())")
        #expect(out.result["arg"].asString == "id")
        #expect(
            out.result["error"].asString
                == "send: id must be a name (non-empty string) or a message id (int), got True",
            "\(out.result.serialize())")
    }

    /// C38: an int `exec_id` that names no open execution is `bad_arg`, not a
    /// silent no-op — ids are minted by the kernel and only ever JOINED.
    @Test func intExecIdNamingNoExecutionIsBadArg() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        var item = Item(AgentId("a"), .object(["type": .string("poke")]))
        item.rawId = .integer(999_999)
        let out = await kernel.send([item])[0]
        #expect(out.pid == nil)
        #expect(out.result["reason"].asString == "bad_arg", "\(out.result.serialize())")
        #expect(out.result["arg"].asString == "id")
        #expect(
            out.result["error"].asString
                == "send: id 999999 names no known execution — ids are minted by the kernel "
                    + "and can be joined, never created",
            "\(out.result.serialize())")
    }

    /// C38: an int `exec_id` that DOES name an open execution joins it — the
    /// same execution, not a second one.
    @Test func intExecIdJoinsAnOpenExecution() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        var opener = Item(AgentId("a"), .object(["type": .string("poke")]), id: "job-9")
        opener.timeout = 5
        let first = await kernel.send([opener])[0]
        var joiner = Item(AgentId("a"), .object(["type": .string("poke")]))
        joiner.rawId = .integer(first.pid!.value)
        let second = await kernel.send([joiner])[0]
        #expect(second.pid == first.pid, "\(second.result.serialize())")
    }

    /// C30/C38: `retries` must be a non-negative int; python's exact "got
    /// <repr>" tail.
    @Test func retriesMustBeANonNegativeInt() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        for bad: JSON in [.integer(-1), .string("soon"), .bool(true), .double(1.5)] {
            var item = Item(AgentId("a"), .object(["type": .string("poke")]))
            item.rawRetries = bad
            let out = await kernel.send([item])[0]
            #expect(out.result["reason"].asString == "bad_arg", "\(bad): \(out.result.serialize())")
            #expect(out.result["arg"].asString == "retries")
            #expect(
                out.result["error"].asString == "send: retries must be a non-negative int, got \(pyRepr(bad))",
                "\(bad): \(out.result.serialize())")
        }
    }

    /// C30/C38: a raw (tag-typed) `timeout` that is not a positive number
    /// refuses with python's exact "got <repr>" tail — a caller relaying an
    /// untyped value (a malformed model arg) is REFUSED, not silently
    /// defaulted to "wait forever".
    @Test func rawTimeoutMustBePositiveWithGotRepr() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        var item = Item(AgentId("a"), .object(["type": .string("poke")]))
        item.rawTimeout = .string("soon")
        let out = await kernel.send([item])[0]
        #expect(out.result["reason"].asString == "bad_arg", "\(out.result.serialize())")
        #expect(out.result["arg"].asString == "timeout")
        #expect(
            out.result["error"].asString
                == "send: timeout must be a positive number of seconds, got 'soon'",
            "\(out.result.serialize())")
    }
}
