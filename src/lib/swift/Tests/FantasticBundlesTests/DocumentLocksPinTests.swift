// file_bridge's per-document locks, pinned.
//
// One lock per (kernel, bridge, path): waiters on ONE key are served in arrival
// order, keys never block each other, a released key with nobody waiting is
// free again at once, and a multi-file edit takes its keys in SORTED order, so
// two transactions naming the same files in opposite orders cannot deadlock.
// A keyed async mutex built to replace this must keep all four.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticFile

private final class Order: @unchecked Sendable {
    private let lock = NSLock()
    private var seen: [String] = []
    func add(_ s: String) { lock.withLock { seen.append(s) } }
    var value: [String] { lock.withLock { seen } }
}

private func bridge() async -> (Kernel, URL) {
    let registry = BundleRegistry()
    registry.register("file_bridge.tools", FileBundle())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("fantastic-doclocks-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
        rootPath: dir.appendingPathComponent(".fantastic"))
    kernel.register(root)
    kernel.setRoot(root)
    _ = await kernel.send([
        Item(
            "kernel_state",
            .object([
                "type": "create_agent", "parent_id": "kernel_state", "id": "fs",
                "handler_module": "file_bridge.tools", "allow": ["."], "root": "sd",
                "ingress_rule": "allow_all_ingress",
            ]))
    ])[0].result
    return (kernel, dir.appendingPathComponent("sd"))
}

private func block(_ old: String, _ new: String) -> String {
    "<<<<<<< SEARCH\n\(old)\n=======\n\(new)\n>>>>>>> REPLACE"
}

private func edit(_ k: Kernel, _ edits: [(String, String, String)]) async -> JSON {
    let blob = edits.map { block($0.1, $0.2) }.joined(separator: "\n")
    let list: [JSON] = edits.map { .object(["path": .string($0.0), "hunks": .integer(1)]) }
    return await k.send([
        Item(
            AgentId("fs"), .object(["type": "edit", "edits": .array(list)]), binary: Data(blob.utf8)
        )
    ])[0].result
}

@Suite("file_bridge — per-document locks")
struct DocumentLocksPinTests {

    @Test func waitersOnOneKeyAreServedInArrivalOrder() async {
        let locks = DocumentLocks()
        let order = Order()
        await locks.acquire("k")
        var tasks: [Task<Void, Never>] = []
        for i in 1...5 {
            tasks.append(
                Task {
                    await locks.acquire("k")
                    order.add("w\(i)")
                    await locks.release("k")
                })
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        // another key is never blocked by "k"
        await locks.acquire("other")
        order.add("other")
        await locks.release("other")
        #expect(order.value == ["other"])
        await locks.release("k")
        for t in tasks { await t.value }
        #expect(order.value == ["other", "w1", "w2", "w3", "w4", "w5"])
        // nobody holds or waits: the key is free at once
        await locks.acquire("k")
        await locks.release("k")
    }

    /// Two transactions over the same two files, named in OPPOSITE orders, run
    /// concurrently many times: every one lands (no deadlock, no lost edit).
    @Test(.timeLimit(.minutes(1)))
    func oppositeOrderMultiFileEditsNeverDeadlock() async {
        let (k, d) = await bridge()
        let rounds = 20
        let left = (0..<rounds).map { "l\($0) = 0" }.joined(separator: "\n") + "\n"
        let right = (0..<rounds).map { "r\($0) = 0" }.joined(separator: "\n") + "\n"
        _ = await k.send([
            Item(
                AgentId("fs"), .object(["type": "write", "path": "a.py"]),
                binary: Data((left + right).utf8))
        ])
        _ = await k.send([
            Item(
                AgentId("fs"), .object(["type": "write", "path": "b.py"]),
                binary: Data((left + right).utf8))
        ])
        await withTaskGroup(of: JSON.self) { group in
            for i in 0..<rounds {
                group.addTask {
                    await edit(
                        k, [("a.py", "l\(i) = 0", "l\(i) = 1"), ("b.py", "l\(i) = 0", "l\(i) = 1")])
                }
                group.addTask {
                    await edit(
                        k, [("b.py", "r\(i) = 0", "r\(i) = 1"), ("a.py", "r\(i) = 0", "r\(i) = 1")])
                }
            }
            for await r in group {
                #expect(r["edited"] == true, "\(r.serialize())")
            }
        }
        let want =
            (0..<rounds).map { "l\($0) = 1" }.joined(separator: "\n") + "\n"
            + (0..<rounds).map { "r\($0) = 1" }.joined(separator: "\n") + "\n"
        #expect(
            (try? String(contentsOf: d.appendingPathComponent("a.py"), encoding: .utf8)) == want)
        #expect(
            (try? String(contentsOf: d.appendingPathComponent("b.py"), encoding: .utf8)) == want)
    }
}
