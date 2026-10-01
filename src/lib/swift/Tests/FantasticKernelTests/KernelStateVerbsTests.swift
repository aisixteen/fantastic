// kernel_state's record verbs on a SESSION loader — every path clamped to the
// running dir, a relative `root` resolved under it (never the process cwd), the
// ids typed as python's `guard_agent_id` types them, and every record write
// seeding the bundle's readme + docs. Twin of python
// `kernel_state/tests/test_kernel_state.py::test_session_loader_serves_sub_namespace`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

import FantasticKernelState

@testable import FantasticKernel

private func ksKernel() -> (Kernel, URL) {
    let reg = BundleRegistry()
    reg.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    let k = Kernel(storage: .inMemory, bundles: reg)
    let workdir = URL(fileURLWithPath: NSTemporaryDirectory())
        .appendingPathComponent("ksverbs-\(UUID().uuidString)")
    let root = Agent(
        id: AgentId(ROOT_ID), handlerModule: ROOT_HANDLER_MODULE, parentId: nil,
        rootPath: workdir.appendingPathComponent(".fantastic"))
    k.register(root)
    k.setRoot(root)
    return (k, workdir)
}

private func send(_ k: Kernel, _ target: String, _ payload: JSON) async -> Outcome {
    await k.send([Item(AgentId(target), payload)])[0]
}

private func session(_ k: Kernel, root: String) async -> String {
    let out = await send(
        k, ROOT_ID,
        .object([
            "type": .string("create_agent"), "parent_id": .string(ROOT_ID),
            "handler_module": .string(ROOT_HANDLER_MODULE), "watch": .bool(false),
            "root": .string(root),
        ]))
    return out.result["id"].asString ?? ""
}

@Suite("kernel_state record verbs")
struct KernelStateVerbsTests {
    @Test func aSessionLoaderServesItsSubNamespaceUnderTheRunningDir() async throws {
        let (k, workdir) = ksKernel()
        defer { try? FileManager.default.removeItem(at: workdir) }
        let sess = ".fantastic/web/s1"
        let sid = await session(k, root: sess)
        let ns = workdir.appendingPathComponent(sess)
        let fm = FileManager.default
        // boot seeds the namespace anchor UNDER THE RUNNING DIR — and, as
        // python's `write_record` does, the bundle's readme and docs beside it.
        #expect(fm.fileExists(atPath: ns.appendingPathComponent("agent.yaml").path))
        #expect(fm.fileExists(atPath: ns.appendingPathComponent("readme.md").path))
        let firstDoc = try #require(RootDocs.all.first?.key)
        #expect(fm.fileExists(atPath: ns.appendingPathComponent("docs/\(firstDoc).md").path))
        // reflect reports the root as configured — relative, as python prints it.
        let r = await send(k, sid, .object(["type": .string("reflect")]))
        #expect(r.result["root"].asString == sess, "\(r.result.serialize())")

        for (id, parent) in [("canvas", sid), ("term1", "canvas")] {
            let p = await send(
                k, sid,
                .object([
                    "type": .string("persist_record"),
                    "record": .object([
                        "id": .string(id), "handler_module": .string("x.ts"),
                        "parent_id": .string(parent),
                    ]),
                ]))
            #expect(p.result == .object(["ok": .bool(true)]), "\(p.result.serialize())")
        }
        let canvas = ns.appendingPathComponent("agents/canvas")
        let term = canvas.appendingPathComponent("agents/term1")
        #expect(fm.fileExists(atPath: term.appendingPathComponent("agent.yaml").path))

        let tree = await send(k, sid, .object(["type": .string("load_tree")]))
        let ids = (tree.result["records"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(Set([sid, "canvas", "term1"]).isSubset(of: Set(ids)), "\(ids)")

        _ = await send(
            k, sid,
            .object([
                "type": .string("forget_record"), "id": .string("term1"),
                "parent_id": .string("canvas"),
            ]))
        #expect(!fm.fileExists(atPath: term.path))
        #expect(fm.fileExists(atPath: canvas.path))
    }

    /// python `fs.Root(workdir, root)`: a root that leaves the running dir
    /// RAISES out of every disk verb.
    @Test func aRootOutsideTheRunningDirRaises() async throws {
        let (k, workdir) = ksKernel()
        defer { try? FileManager.default.removeItem(at: workdir) }
        let sid = await session(k, root: ".fantastic/web/s2")
        _ = await send(
            k, ROOT_ID,
            .object([
                "type": .string("update_agent"), "id": .string(sid),
                "root": .string("../escape"),
            ]))
        for payload: JSON in [
            .object(["type": .string("load_tree")]),
            .object([
                "type": .string("persist_record"),
                "record": .object(["id": .string("x"), "parent_id": .string(sid)]),
            ]),
            .object(["type": .string("forget_record"), "id": .string("x")]),
        ] {
            let out = await send(k, sid, payload)
            #expect(out.status == .failed, "\(payload.serialize()): \(out.result.serialize())")
            #expect(
                out.result["error"].asString
                    == "ValueError: fs: root '../escape' escapes the running dir",
                "\(out.result.serialize())")
        }
        #expect(
            !FileManager.default.fileExists(
                atPath: workdir.deletingLastPathComponent().appendingPathComponent("escape").path))
    }

    /// python `guard_agent_id`: a truthy id that is not a string is `id must be
    /// a non-empty string`, and so is a non-string `parent_id`.
    @Test func recordIdsAreTypedLikePython() async {
        let (k, workdir) = ksKernel()
        defer { try? FileManager.default.removeItem(at: workdir) }
        let sid = await session(k, root: ".fantastic/web/s3")
        let cases: [(JSON, String)] = [
            (
                .object(["type": .string("persist_record"), "record": .object(["id": .integer(5)])]
                ),
                "persist_record: id must be a non-empty string"
            ),
            (
                .object([
                    "type": .string("persist_record"),
                    "record": .object(["id": .string("x"), "parent_id": .integer(5)]),
                ]),
                "persist_record: id must be a non-empty string"
            ),
            (
                .object(["type": .string("forget_record"), "id": .integer(5)]),
                "forget_record: id must be a non-empty string"
            ),
            (
                .object([
                    "type": .string("forget_record"), "id": .string("x"), "parent_id": .integer(5),
                ]),
                "forget_record: id must be a non-empty string"
            ),
        ]
        for (payload, text) in cases {
            let out = await send(k, sid, payload)
            #expect(out.result["reason"].asString == "bad_arg", "\(out.result.serialize())")
            #expect(out.result["error"].asString == text, "\(out.result.serialize())")
        }
        #expect(
            !FileManager.default.fileExists(
                atPath: workdir.appendingPathComponent(".fantastic/web/s3/agents/x").path))
    }

    /// python's `VERBS` has no `shutdown`: a direct one is an unknown type.
    @Test func shutdownIsAnUnknownType() async {
        let (k, workdir) = ksKernel()
        defer { try? FileManager.default.removeItem(at: workdir) }
        let sid = await session(k, root: ".fantastic/web/s4")
        let out = await send(k, sid, .object(["type": .string("shutdown")]))
        #expect(out.status == .errored, "\(out.result.serialize())")
        #expect(
            out.result["error"].asString == "kernel_state: unknown type 'shutdown'",
            "\(out.result.serialize())")
        #expect(out.result["reason"].asString == "unhandled")
    }
}
