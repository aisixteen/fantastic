// The REPLY SHAPES a kernel verb answers with, graded against python.
//
// Python is the spec for every one of these: `{updated, id, agent}` rather than
// the bare record, a coded refusal rather than a bare `{error}` (`delete_agent`
// on an unknown/absent id, `cancel` with no task_id, an unhandled system verb),
// seven keys on a ledger row, and python's prose on the `not_here` code. A
// caller that branches on `reason`, reads `updated`, or diffs a row's key set
// gets the same answer on every runtime.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

private struct Noop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
}

/// Answers whatever `reply` the payload names — the fixture for the settle
/// truthiness cases below.
private struct Replier: AgentBundle {
    let name = "replier"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        payload["reply"]
    }
}

@Suite("K-D: kernel-verb reply shapes are python's")
struct ReplyShapeTests {
    private func makeKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("noop.tools", Noop())
        registry.register("replier.tools", Replier())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    private func call(_ k: Kernel, _ target: String, _ payload: JSON) async -> JSON {
        await k.send([Item(AgentId(target), payload)])[0].result
    }

    @discardableResult
    private func mkChild(_ k: Kernel, _ id: String) async -> JSON {
        let r = await call(
            k, "kernel",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel"),
                "handler_module": .string("noop.tools"), "id": .string(id),
            ]))
        #expect(r["id"].asString == id, "fixture create failed: \(r.serialize())")
        return r
    }

    /// `{updated:true, id, agent}` — not the bare record. A caller taught by the
    /// kernel_verbs doc branches on `updated`; the record has no such field, so
    /// the call that SUCCEEDED read as a failure and the model retried it.
    @Test func updateAgentAnswersUpdatedIdAndTheRecord() async {
        let k = makeKernel()
        await mkChild(k, "kid")
        let r = await call(
            k, "kernel",
            .object([
                "type": .string("update_agent"), "id": .string("kid"),
                "note": .string("hello"),
            ]))
        #expect(r["updated"].asBool == true, "\(r.serialize())")
        #expect(r["id"].asString == "kid", "\(r.serialize())")
        // The record rides UNDER `agent`, whole.
        #expect(r["agent"]["id"].asString == "kid", "\(r.serialize())")
        #expect(r["agent"]["note"].asString == "hello", "\(r.serialize())")
        // And the three keys are the whole reply, so a key-set diff against
        // python holds.
        #expect(Array(r.asObject?.keys ?? []) == ["updated", "id", "agent"], "\(r.serialize())")
    }

    /// An unknown id is `unknown_agent` + `deleted:false` — a code to branch on
    /// and the LIVE id index to correct from.
    @Test func deleteAgentOnAnUnknownIdIsTheUnknownAgentRefusal() async {
        let k = makeKernel()
        await mkChild(k, "kid")
        let r = await call(
            k, "kernel", .object(["type": .string("delete_agent"), "id": .string("typo")]))
        #expect(r["reason"].asString == "unknown_agent", "\(r.serialize())")
        #expect(r["deleted"].asBool == false, "\(r.serialize())")
        let ids = (r["agents"].asArray ?? []).compactMap { $0.asString }
        #expect(ids.contains("kid"), "\(r.serialize())")
        #expect(ids.contains("kernel_state"), "\(r.serialize())")
    }

    /// No `id` at all is `missing_arg`, carrying the ARG — the machine field is
    /// the whole point of the code.
    @Test func deleteAgentWithoutAnIdIsMissingArg() async {
        let k = makeKernel()
        let r = await call(k, "kernel", .object(["type": .string("delete_agent")]))
        #expect(r["reason"].asString == "missing_arg", "\(r.serialize())")
        #expect(r["arg"].asString == "id", "\(r.serialize())")
        #expect(r["error"].asString == "delete_agent: id required", "\(r.serialize())")
    }

    /// Same for `cancel` — `{cancelled:false}` would claim the kernel looked; it
    /// never did, because nothing was named.
    @Test func cancelWithoutATaskIdIsMissingArg() async {
        let k = makeKernel()
        let r = await call(k, "kernel", .object(["type": .string("cancel")]))
        #expect(r["reason"].asString == "missing_arg", "\(r.serialize())")
        #expect(r["arg"].asString == "task_id", "\(r.serialize())")
        #expect(r["error"].asString == "cancel: task_id required", "\(r.serialize())")
    }

    /// An unhandled SYSTEM verb is `unhandled` + the `verb` that was asked —
    /// never a reason-less `{error}`, which the reflect flags would decorate into
    /// something that reads like a healthy reflect.
    @Test func anUnhandledSystemVerbCarriesReasonAndVerb() async throws {
        let k = makeKernel()
        let r = try await k.handleSystemVerb(
            target: k.root!, verb: "no_such_system_verb",
            payload: .object(["type": .string("no_such_system_verb")]))
        #expect(r["reason"].asString == "unhandled", "\(r.serialize())")
        #expect(r["verb"].asString == "no_such_system_verb", "\(r.serialize())")
    }

    /// `not_here` says PYTHON'S sentence and names the verb in a machine field.
    /// One code must not mean two prose answers across runtimes, and a caller
    /// should not have to parse the sentence to learn which verb it misaddressed.
    @Test func aKernelVerbAtAChildSaysPythonsWordsAndNamesTheVerb() async {
        let k = makeKernel()
        await mkChild(k, "kid")
        let r = await call(k, "kid", .object(["type": .string("ps")]))
        #expect(r["reason"].asString == "not_here", "\(r.serialize())")
        #expect(r["verb"].asString == "ps", "\(r.serialize())")
        #expect(
            r["error"].asString == "ps is a kernel verb — kid does not answer it",
            "\(r.serialize())")
        #expect(
            r["hint"].asString == "address the kernel root ('kernel'); name the agent "
                + "you mean in `parent_id` / `id`", "\(r.serialize())")
    }

    /// `list_agents` walks the registry in REGISTRATION order (python
    /// pages `self.ctx.agents.values()` with no sort).
    @Test func testListAgentsIsRegistryOrder() async {
        let k = makeKernel()
        for id in ["zeta", "alpha", "mid"] { _ = await mkChild(k, id) }
        let r = await call(k, "kernel", .object(["type": .string("list_agents")]))
        let ids = (r["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids == ["kernel_state", "zeta", "alpha", "mid"], "\(r.serialize())")
    }

    /// The reply is a RECORD, built as an ordered map: `id`,
    /// `handler_module`, the meta in the order it was given, then `parent_id`.
    /// Routed through `JSONEncoder` it came back in a different order every
    /// process, nested grant maps included.
    @Test func createAgentReplyKeyOrderIsStableAcrossRuns() async {
        let k = makeKernel()
        let r = await call(
            k, "kernel",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("noop.tools"), "id": .string("kid"),
                "display_name": .string("Kid"),
                "can_call": .object([
                    "$self": .string("*"), "$parent": .string("*"),
                    "$subtree": .string("*"), "kernel_state": .array([.string("ps")]),
                ]),
            ]))
        #expect(
            r.asObject?.keys.elements == [
                "id", "handler_module", "display_name", "can_call", "callable_by", "parent_id",
            ], "\(r.serialize())")
        #expect(
            r["can_call"].asObject?.keys.elements == ["$self", "$parent", "$subtree", "kernel_state"],
            "\(r.serialize())")
        // The same record read back through `list_agents` keeps that order.
        let listed = await call(k, "kernel", .object(["type": .string("list_agents")]))
        let row = (listed["agents"].asArray ?? []).first { $0["id"].asString == "kid" }
        #expect(row?.asObject?.keys.elements == r.asObject?.keys.elements, "\(listed.serialize())")
    }

    /// A `ledger` row is python's SEVEN keys. `verb` was an eighth that only the
    /// ports carried; it still rides the durable sidecar (where a joiner is
    /// authorized against it after a reboot), just not the read verb.
    @Test func ledgerRowsCarryNoVerbKey() async {
        let k = makeKernel()
        await mkChild(k, "kid")
        // One NAMED execution, so the row is settled and carries an outcome.
        _ = await k.send([Item(AgentId("kid"), .object(["type": .string("poke")]), id: "job-1")])
        let r = await call(k, "kernel", .object(["type": .string("ledger")]))
        let rows = r["executions"].asArray ?? []
        #expect(!rows.isEmpty, "\(r.serialize())")
        for row in rows {
            let keys = Array(row.asObject?.keys ?? [])
            #expect(!keys.contains("verb"), "row carries `verb`: \(row.serialize())")
            for want in ["id", "target", "status", "attempts", "retries", "named"] {
                #expect(keys.contains(want), "row is missing \(want): \(row.serialize())")
            }
        }
    }

    /// Python settles `ERRORED if reply.get("error") else COMPLETED`, so a
    /// reply whose `error` is `0`, `0.0`, `[]` or `{}` settles COMPLETED. Swift
    /// read anything but a bool or a string as truthy and settled every one of
    /// them `errored`, so a handler answering `{"error": 0}` — a status code, a
    /// count — was reported as a failure.
    @Test func aFalsyNonStringErrorSettlesCompleted() async {
        let k = makeKernel()
        _ = await call(
            k, "kernel",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel"),
                "handler_module": .string("replier.tools"), "id": .string("rep"),
            ]))
        let falsy: [JSON] = [
            .integer(0), .double(0), .array([]), .object([:]), .string(""), .bool(false),
            .null,
        ]
        for value in falsy {
            let out = await k.send([
                Item(AgentId("rep"), .object(["type": .string("say"), "reply": .object(["error": value])]))
            ])[0]
            #expect(out.status == .completed, "error \(value.serialize()) settled \(out.status)")
        }
        let truthy: [JSON] = [
            .integer(1), .double(0.5), .array([.string("x")]), .object(["a": .integer(1)]),
            .string("boom"), .bool(true),
        ]
        for value in truthy {
            let out = await k.send([
                Item(AgentId("rep"), .object(["type": .string("say"), "reply": .object(["error": value])]))
            ])[0]
            #expect(out.status == .errored, "error \(value.serialize()) settled \(out.status)")
        }
    }
}
