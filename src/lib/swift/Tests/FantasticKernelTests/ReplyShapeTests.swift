// K-D — the REPLY SHAPES a kernel verb answers with, graded against python.
//
// Python is the spec for every one of these. Each case here is a measured
// divergence from the 2026-09-02 sweep: swift answered the bare record where
// python answers `{updated, id, agent}`, a bare `{error}` where python answers a
// coded refusal (`delete_agent` on an unknown/absent id, `cancel` with no
// task_id, an unhandled system verb), an eighth `verb` key on a ledger row
// python renders with seven, and its own prose on the `not_here` code. A caller
// that branches on `reason`, or reads `updated`, or diffs a row's key set, got a
// different answer per runtime for the same call — the exact thing the one
// refusal shape exists to prevent.

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

@Suite("K-D: kernel-verb reply shapes are python's")
struct ReplyShapeTests {
    private func makeKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("noop.tools", Noop())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "core", handlerModule: nil, parentId: nil)
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
    /// and the LIVE id index to correct from. The bare `{error: "no agent"}` this
    /// used to answer is the measured 34-retry case.
    @Test func deleteAgentOnAnUnknownIdIsTheUnknownAgentRefusal() async {
        let k = makeKernel()
        await mkChild(k, "kid")
        let r = await call(
            k, "kernel", .object(["type": .string("delete_agent"), "id": .string("typo")]))
        #expect(r["reason"].asString == "unknown_agent", "\(r.serialize())")
        #expect(r["deleted"].asBool == false, "\(r.serialize())")
        let ids = (r["agents"].asArray ?? []).compactMap { $0.asString }
        #expect(ids.contains("kid"), "\(r.serialize())")
        #expect(ids.contains("core"), "\(r.serialize())")
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
    @Test func anUnhandledSystemVerbCarriesReasonAndVerb() async {
        let k = makeKernel()
        let r = await k.handleSystemVerb(
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
            r["hint"].asString == "address the kernel root (alias 'kernel'); name the agent "
                + "you mean in `parent_id` / `id`", "\(r.serialize())")
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
}
