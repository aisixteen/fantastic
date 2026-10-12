// ram_store under CONCURRENT callers — a swift-only test (python's store runs on
// one asyncio thread and needs no lock; swift dispatches on a thread pool).
//
// Many agents append / read / search on ONE ram_store while
// the operator dumps it and the kernel's FLUSHER appends (and, over a small
// `ledger_cap`, prunes) its execution rows into it. Every verb must see a
// consistent table: no crash, every append lands exactly once, every writer
// reads back exactly its own rows.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel
@testable import FantasticKernelState
@testable import FantasticRamStore

private let WRITERS = 16
private let ROUNDS = 120

private func obj(_ pairs: OrderedDictionary<String, JSON>) -> JSON { .object(pairs) }

private func send(_ k: Kernel, _ payload: JSON, as sender: String?) async -> Outcome {
    let item = Item(AgentId("memory"), payload)
    return await KernelTaskLocals.$sender.withValue(sender.map { AgentId($0) }) {
        await k.send([item])[0]
    }
}

/// A started kernel whose root names a ram_store `memory`, and `WRITERS` bare
/// agents granted both ways to call it.
private func stressKernel() async -> Kernel {
    let r = BundleRegistry()
    r.register("kernel_state.tools", KernelStateBundle())
    r.register("ram_store.tools", RamStoreBundle())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: "kernel_state.tools", parentId: nil)
    k.register(root)
    k.setRoot(root)
    var callableBy: OrderedDictionary<String, JSON> = [:]
    for w in 0..<WRITERS {
        let a = Agent(
            id: AgentId("w\(w)"), handlerModule: nil, parentId: root.id,
            meta: ["can_call": .object(["memory": .string("*")])])
        k.register(a)
        root.insertChild(a)
        callableBy["w\(w)"] = .string("*")
    }
    let made = await k.send([
        Item(
            "kernel_state",
            obj([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "id": .string("memory"), "handler_module": .string("ram_store.tools"),
                "callable_by": .object(callableBy),
            ]))
    ])[0].result
    #expect(made["id"].asString == "memory", "\(made.serialize())")
    k.update(root.id, ["memory_store_id": .string("memory")])
    // A small window, so the flusher's PRUNE runs against the store mid-stress too.
    k.ledger.setCap(50)
    let started = await k.start(bootAll: false)
    #expect(started["started"].asBool == true, "\(started.serialize())")
    return k
}

@Suite("ram_store — concurrent callers on one store")
struct RamStoreConcurrencyTests {
    @Test(.timeLimit(.minutes(2)))
    func concurrentAppendReadSearchDumpAndFlush() async {
        let k = await stressKernel()
        let failures = await withTaskGroup(of: [String].self, returning: [String].self) { group in
            for w in 0..<WRITERS {
                group.addTask {
                    let me = "w\(w)"
                    var bad: [String] = []
                    for i in 0..<ROUNDS {
                        let app = await send(
                            k,
                            obj([
                                "type": .string("append"),
                                "rows": .array([
                                    obj([
                                        "role": .string("user"),
                                        "content": .string("\(me) round \(i) alpha"),
                                    ])
                                ]),
                            ]), as: me)
                        if app.result["appended"].asInt != 1 {
                            bad.append("\(me) append \(i): \(app.result.serialize())")
                        }
                        let rd = await send(
                            k,
                            obj([
                                "type": .string("read"), "limit": .integer(20),
                                "order": .string("desc"),
                            ]), as: me)
                        if pyTruthy(rd.result["error"]) {
                            bad.append("\(me) read \(i): \(rd.result.serialize())")
                        }
                        if i % 4 == 0 {
                            let s = await send(
                                k, obj(["type": .string("search"), "query": .string("\(me) round")]),
                                as: me)
                            if pyTruthy(s.result["error"]) {
                                bad.append("\(me) search \(i): \(s.result.serialize())")
                            }
                        }
                    }
                    return bad
                }
            }
            // The operator dumps the whole store while it is written.
            for d in 0..<2 {
                group.addTask {
                    var bad: [String] = []
                    for i in 0..<ROUNDS {
                        let out = await send(k, obj(["type": .string("dump")]), as: nil)
                        if out.result["tables"]["rows"].asArray == nil {
                            bad.append("dump \(d).\(i): \(out.result.serialize())")
                        }
                    }
                    return bad
                }
            }
            // The kernel's own writes: flushes (and the prune past the cap).
            group.addTask {
                for _ in 0..<ROUNDS { _ = await k.flushExecutions() }
                return []
            }
            var all: [String] = []
            for await b in group { all += b }
            return all
        }
        #expect(failures.isEmpty, "\(failures.prefix(5))")
        // Every writer reads back exactly its own rows, each once.
        for w in 0..<WRITERS {
            let me = "w\(w)"
            let rows = await send(k, obj(["type": .string("read")]), as: me).result["rows"].asArray ?? []
            let contents = rows.compactMap { $0["content"].asString }
            #expect(contents.count == ROUNDS, "\(me): \(contents.count) rows")
            #expect(Set(contents) == Set((0..<ROUNDS).map { "\(me) round \($0) alpha" }))
            let ids = rows.compactMap { $0["id"].asInt }
            #expect(ids == ids.sorted() && Set(ids).count == ids.count, "\(me) ids out of order")
        }
        // The kernel wrote into the same store meanwhile: its own rows are there.
        let guid = k.root?.metaValue(forKey: "guid")?.asString ?? ""
        let dumped = await send(k, obj(["type": .string("dump")]), as: nil).result
        let kernelRows = (dumped["tables"]["rows"].asArray ?? []).filter {
            $0["owner"].asString == "\(guid)|kernel"
        }
        #expect(kernelRows.contains { $0["row"]["kind"].asString == "flush" })
        await k.stopFlusher()
    }
}
