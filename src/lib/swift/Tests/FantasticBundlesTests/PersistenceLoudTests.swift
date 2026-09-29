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
