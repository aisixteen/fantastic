// A SESSION loader (a `kernel_state` with a `root`, under a ws leg) persists a
// federated kernel's records — a canvas and the thin agents nested under it.
// Those records are never live on the host, so the record-owner walk and the
// path resolution climb THIS LOADER'S persisted records (disk position) past the
// live tree: nested 2+ levels is allowed for the owning leg and refused for a
// sibling, and an existing record is placed where it is — never by the
// payload's parent.
//
// Twin of python `bundled_agents/loader/kernel_state/tests/test_nested_records.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

import FantasticKernelState

@testable import FantasticKernel

/// A bare leg: a registered bundle that answers nothing but `boot`.
private struct Leg: AgentBundle {
    let name = "leg"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        nil
    }
}

private struct Fixture {
    let k: Kernel
    /// The session loader's namespace (python `ROOT = ".fantastic/web"`).
    let root: URL

    func exists(_ rel: String) -> Bool {
        FileManager.default.fileExists(atPath: root.appendingPathComponent(rel).path)
    }
}

private func call(_ k: Kernel, _ target: String, _ payload: JSON, as sender: String? = nil) async
    -> JSON
{
    let item = Item(AgentId(target), payload)
    return await KernelTaskLocals.$currentSender.withValue(sender.map { AgentId($0) }) {
        await k.send([item])[0].result
    }
}

private func setup() async -> Fixture {
    let reg = BundleRegistry()
    reg.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    reg.register("leg.tools", Leg())
    let k = Kernel(storage: .inMemory, bundles: reg)
    let rootAgent = Agent(id: AgentId(ROOT_ID), handlerModule: ROOT_HANDLER_MODULE, parentId: nil)
    k.register(rootAgent)
    k.setRoot(rootAgent)
    let root = URL(fileURLWithPath: NSTemporaryDirectory())
        .appendingPathComponent("nested-\(UUID().uuidString)")
        .appendingPathComponent(".fantastic/web")
    let recs: [OrderedDictionary<String, JSON>] = [
        // the loader's record verbs are privileged: the owning leg names them
        [
            "id": .string("wws"), "handler_module": .string("leg.tools"),
            "parent_id": .string(ROOT_ID),
            "can_call": .object([
                "web_loader": .array([
                    .string("persist_record"), .string("forget_record"), .string("load_tree"),
                ])
            ]),
        ],
        [
            "id": .string("rest_leg"), "handler_module": .string("leg.tools"),
            "parent_id": .string(ROOT_ID),
            "can_call": .object(["web_loader": .string("*")]),
        ],
        [
            "id": .string("web_loader"), "handler_module": .string(ROOT_HANDLER_MODULE),
            "parent_id": .string("wws"), "root": .string(root.path), "watch": .bool(false),
            "callable_by": .object(["rest_leg": .string("*")]),
        ],
    ]
    for rec in recs {
        var p: OrderedDictionary<String, JSON> = ["type": .string("create_agent")]
        for (kk, v) in rec { p[kk] = v }
        let out = await call(k, ROOT_ID, .object(p))
        #expect(out["id"] == rec["id"], "\(out.serialize())")
    }
    return Fixture(k: k, root: root)
}

private func persist(_ f: Fixture, _ who: String, _ rid: String, _ parent: String) async -> JSON {
    await call(
        f.k, "web_loader",
        .object([
            "type": .string("persist_record"),
            "record": .object(["id": .string(rid), "parent_id": .string(parent)]),
        ]), as: who)
}

private let OK: JSON = .object(["ok": .bool(true)])

@Suite("kernel_state — records nested under a federated record")
struct NestedRecordsTests {
    @Test func theOwningLegPersistsRecordsNestedUnderAFederatedRecord() async {
        let f = await setup()
        #expect(await persist(f, "wws", "canvas", "web_loader") == OK)
        #expect(await persist(f, "wws", "terminal_view", "canvas") == OK)
        #expect(await persist(f, "wws", "deep", "terminal_view") == OK)
        #expect(
            f.exists("agents/canvas/agents/terminal_view/agents/deep/agent.yaml"),
            "a 2-level nested record lands under its real parents")
        let tree = await call(f.k, "web_loader", .object(["type": .string("load_tree")]))
        var parents: [String: JSON] = [:]
        for r in tree["records"].asArray ?? [] {
            if let id = r["id"].asString { parents[id] = r["parent_id"] }
        }
        #expect(parents["terminal_view"] == .string("canvas"), "\(tree.serialize())")
        #expect(parents["deep"] == .string("terminal_view"), "\(tree.serialize())")
    }

    @Test func aSiblingMayNotTouchTheNestedRecords() async {
        let f = await setup()
        _ = await persist(f, "wws", "canvas", "web_loader")
        _ = await persist(f, "wws", "terminal_view", "canvas")
        var r = await persist(f, "rest_leg", "sneak", "terminal_view")
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        r = await call(
            f.k, "web_loader",
            .object([
                "type": .string("forget_record"), "id": .string("terminal_view"),
                "parent_id": .string("canvas"),
            ]), as: "rest_leg")
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(f.exists("agents/canvas/agents/terminal_view/agent.yaml"))
    }

    /// Naming a record that exists with `parent_id` = yourself used to pass the
    /// guard (the payload's parent was walked first) and overwrite it.
    @Test func anExistingRecordIsPlacedByItsPositionNotThePayload() async {
        let f = await setup()
        _ = await persist(f, "wws", "canvas", "web_loader")
        _ = await persist(f, "wws", "terminal_view", "canvas")
        let r = await persist(f, "rest_leg", "terminal_view", "rest_leg")
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(!f.exists("agents/terminal_view"))
    }

    @Test func forgetRecordRemovesANestedRecordWhereItIs() async {
        let f = await setup()
        _ = await persist(f, "wws", "canvas", "web_loader")
        _ = await persist(f, "wws", "terminal_view", "canvas")
        _ = await persist(f, "wws", "deep", "terminal_view")
        let r = await call(
            f.k, "web_loader", .object(["type": .string("forget_record"), "id": .string("deep")]),
            as: "wws")
        #expect(r == OK, "\(r.serialize())")
        #expect(!f.exists("agents/canvas/agents/terminal_view/agents/deep"))
        #expect(f.exists("agents/canvas/agents/terminal_view/agent.yaml"))
    }

    /// No fallback: a brand-new record's placement comes from the payload, and
    /// the walk from there must still reach the caller.
    @Test func anIdInNeitherTreeNorDiskIsPlacedByThePayloadAndGuarded() async {
        let f = await setup()
        let r = await persist(f, "wws", "orphan", "nowhere")
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
    }
}
