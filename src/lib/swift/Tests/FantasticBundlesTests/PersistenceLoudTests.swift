// Persistence is LOUD (audit 2026-09-28, d2) and the ROOT loader's tree is the
// operator's (a15). Python `kernel_state/tests/test_persistence_loud.py` and
// `test_blob.py::test_persist_record_refuses_a_siblings_namespace` are the oracles.

import FantasticFile
import FantasticJSON
import FantasticKernel
import FantasticKernelState
import Foundation
import OrderedCollections
import Testing

private func diskKernel() -> (Kernel, URL) {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("fantastic-loud-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return (makeDiskKernel(workdir: dir), dir)
}

private func persistence(_ k: Kernel) async -> JSON {
    await k.send([Item("kernel_state", ["type": "reflect"])])[0].result["persistence"]
}

private func createStore(_ k: Kernel, _ meta: OrderedDictionary<String, JSON>) async {
    var create: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state", "id": "store",
        "handler_module": "file_bridge.tools", "root": ".fantastic",
        "ingress_rule": "allow_all_ingress",
    ]
    for (key, v) in meta { create[key] = v }
    let r = await k.send([Item("kernel_state", .object(create))])[0].result
    #expect(r["id"] == "store", "\(r.serialize())")
}

@Suite("persistence is loud")
struct PersistenceLoudTests {
    @Test func aStoreThatDoesNotExposeTheRecordsIsLoudAtBoot() async throws {
        let (k, dir) = diskKernel()
        defer { try? FileManager.default.removeItem(at: dir) }
        await createStore(k, ["allow": ["data"]])
        let lines = await RuntimeLog.capture {
            _ = await k.send([Item("kernel_state", ["type": "boot"])])
        }
        let p = await persistence(k)
        #expect(p["provider"].isNull, "\(p.serialize())")
        #expect(p["error"]["store"] == "store", "\(p.serialize())")
        #expect(p["error"]["path"].asString?.contains("agent.yaml") == true, "\(p.serialize())")
        #expect(lines.joined().contains("persist through 'store' failed"), "\(lines)")
        await k.shutdown()
    }

    @Test func aRefusedWriteIsLoudAndClearsWhenWritesLand() async throws {
        let (k, dir) = diskKernel()
        defer { try? FileManager.default.removeItem(at: dir) }
        await createStore(k, ["allow": ["."], "readonly": true])
        _ = await k.send([Item("kernel_state", ["type": "boot"])])
        #expect(await persistence(k) == ["provider": "store"])
        let lines = await RuntimeLog.capture {
            _ = await k.send([
                Item(
                    "kernel_state",
                    [
                        "type": "create_agent", "parent_id": "kernel_state", "id": "probe",
                        "handler_module": "file_bridge.tools", "allow": ["."],
                    ])
            ])
            await k.flushRecords()
        }
        let p = await persistence(k)
        #expect(p["provider"].isNull && p["error"]["store"] == "store", "\(p.serialize())")
        #expect(p["error"]["error"].asString?.contains("readonly") == true, "\(p.serialize())")
        #expect(lines.joined().contains("persist through 'store' failed"), "\(lines)")
        // the operator fixes the store; the next write (the store's own updated
        // record) lands and the error clears
        k.update(AgentId("store"), ["readonly": false])
        await k.flushRecords()
        #expect(await persistence(k) == ["provider": "store"])
        #expect(
            FileManager.default.fileExists(
                atPath: dir.appendingPathComponent(".fantastic/agents/store/agent.yaml").path))
        await k.shutdown()
    }
}

@Suite("the root loader's tree is the operator's")
struct RootLoaderOwnTreeTests {
    /// a15: no AGENT writes the root loader's tree — not a sibling's record and
    /// not its own. Named grants, so the LOADER's guard answers, not the gate.
    @Test func recordVerbsFromAnAgentAreRefused() async {
        let kernel = makeKernelWithAll()
        let made = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state", "id": "someone",
                    "handler_module": "file_bridge.tools",
                    "can_call": ["kernel_state": ["persist_record", "forget_record"]],
                ])
        ])[0].result
        #expect(made["id"] == "someone", "\(made.serialize())")
        let payloads: [JSON] = [
            ["type": "persist_record", "record": ["id": "noted", "parent_id": "kernel_state"]],
            ["type": "forget_record", "id": "noted"],
        ]
        for payload in payloads {
            let out = await KernelTaskLocals.$currentSender.withValue(AgentId("someone")) {
                await kernel.send([Item("kernel_state", payload)])[0].result
            }
            #expect(out["reason"] == "unauthorized", "\(out.serialize())")
            #expect(
                out["error"].asString?.contains("only by the operator") == true,
                "\(out.serialize())")
            #expect(out["side"] == "store", "\(out.serialize())")
        }
    }
}

@Suite("record writes through the store")
struct RecordWritesThroughStoreTests {
    /// python `_persist_via_store` pages the merge read to EOF with `offset`, so
    /// a record over one chunk (64 KiB) keeps its unknown keys on the next write.
    @Test func aRecordOverOneChunkKeepsItsUnknownKeys() async throws {
        let (k, dir) = diskKernel()
        defer { try? FileManager.default.removeItem(at: dir) }
        await createStore(k, ["allow": ["."]])
        _ = await k.send([Item("kernel_state", ["type": "boot"])])
        _ = await k.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state", "id": "big",
                    "handler_module": "file_bridge.tools", "allow": ["."],
                ])
        ])
        await k.flushRecords()
        let file = dir.appendingPathComponent(".fantastic/agents/big/agent.yaml")
        // A sidecar key written beside the record, far past one chunk.
        let pad = String(repeating: "z", count: 100_000)
        var text = try String(contentsOf: file, encoding: .utf8)
        text += "sidecar: \(pad)\n"
        try text.write(to: file, atomically: true, encoding: .utf8)
        k.update(AgentId("big"), ["x": 1])
        await k.flushRecords()
        let after = try String(contentsOf: file, encoding: .utf8)
        #expect(after.contains("sidecar: \(pad)"), "the unknown key was dropped on the merge")
        #expect(after.contains("x: 1"))
        // Each page names its offset, as python's merge read does.
        let offsets = k.rows.all().filter {
            $0["agent"]?.asString == "store" && $0["verb"]?.asString == "read_stream"
                && $0["payload"]?["path"].asString == "agents/big/agent.yaml"
        }.compactMap { $0["payload"]?["offset"].asInt }
        #expect(offsets.contains(0) && offsets.contains(65536), "\(offsets)")
        await k.shutdown()
    }

    /// python seeds the sidecars ONLY after the write landed — and seeds the
    /// bundle's `docs/` tree with its readme.
    @Test func onlyALandedWriteSeedsTheReadmeAndDocs() async throws {
        let fm = FileManager.default
        for landed in [true, false] {
            let (k, dir) = diskKernel()
            defer { try? fm.removeItem(at: dir) }
            // Refused: the record's own path is not exposed, its sidecars are.
            let allow: JSON =
                landed
                ? ["."] : ["agent.yaml", "agents/probe/readme.md", "agents/probe/docs"]
            await createStore(k, ["allow": allow])
            _ = await k.send([Item("kernel_state", ["type": "boot"])])
            _ = await k.send([
                Item(
                    "kernel_state",
                    [
                        "type": "create_agent", "parent_id": "kernel_state", "id": "probe",
                        "handler_module": .string(ROOT_HANDLER_MODULE), "watch": false,
                    ])
            ])
            _ = await RuntimeLog.capture { await k.flushRecords() }
            let agentDir = dir.appendingPathComponent(".fantastic/agents/probe")
            let firstDoc = try #require(RootDocs.all.first?.key)
            #expect(
                fm.fileExists(atPath: agentDir.appendingPathComponent("readme.md").path)
                    == landed, "landed=\(landed)")
            #expect(
                fm.fileExists(atPath: agentDir.appendingPathComponent("docs/\(firstDoc).md").path)
                    == landed, "landed=\(landed)")
            await k.shutdown()
        }
    }
}
