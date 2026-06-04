// apple_kvs tests — deterministic via injected seams: a MemoryKVSBacking (no
// dependency on real iCloud / entitlement) + an availability override (no
// dependency on the host's iCloud sign-in / network). We drive the bundle
// through a real in-memory Kernel so watch/emit fan-out is exercised for real.

import FantasticAppleKVS
import FantasticJSON
import FantasticKernel
import Foundation
import Testing

@Suite("apple_kvs")
struct AppleKVSTests {

    // Build a kernel whose `apple_kvs.tools` is the injected (deterministic)
    // bundle, with a created `kv` agent ready for verb dispatch.
    private func bootKV(_ bundle: AppleKVSBundle) async -> (Kernel, AgentId) {
        let reg = BundleRegistry()
        reg.register("apple_kvs.tools", bundle)
        let kernel = Kernel(storage: .inMemory, bundles: reg)
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("apple_kvs.tools"),
                "id": .string("kv"),
            ]))
        return (kernel, AgentId("kv"))
    }

    private func available() -> AppleKVSBundle {
        AppleKVSBundle(
            store: MemoryKVSBacking(),
            manager: AppleKVSManager(availabilityOverride: { (true, nil) }))
    }

    @Test func roundTripSetGetDelete() async throws {
        let (kernel, kv) = await bootKV(available())
        var r = await kernel.send(
            kv,
            .object([
                "type": .string("set"), "key": .string("nodes.a"),
                "value": .object(["host": .string("h1"), "port": .integer(9)]),
            ]))
        #expect(r["set"].asBool == true)

        r = await kernel.send(kv, .object(["type": .string("read"), "key": .string("nodes.a")]))
        #expect(r["value"]["host"].asString == "h1")
        #expect(r["value"]["port"].asInt == 9)

        r = await kernel.send(kv, .object(["type": .string("read"), "key": .string("nope")]))
        #expect(r["value"].isNull)

        r = await kernel.send(kv, .object(["type": .string("delete"), "key": .string("nodes.a")]))
        #expect(r["deleted"].asBool == true)
        r = await kernel.send(kv, .object(["type": .string("read"), "key": .string("nodes.a")]))
        #expect(r["value"].isNull)
    }

    @Test func listFiltersByPrefix() async throws {
        let (kernel, kv) = await bootKV(available())
        for (k, v) in [("nodes.a", "1"), ("nodes.b", "2"), ("recents.x", "3")] {
            _ = await kernel.send(
                kv,
                .object(["type": .string("set"), "key": .string(k), "value": .string(v)]))
        }
        var r = await kernel.send(
            kv, .object(["type": .string("list"), "prefix": .string("nodes.")]))
        let nodeKeys = (r["keys"].asArray ?? []).map { $0["key"].asString ?? "" }
        #expect(nodeKeys == ["nodes.a", "nodes.b"])
        for item in r["keys"].asArray ?? [] { #expect((item["size"].asInt ?? -1) >= 0) }

        r = await kernel.send(kv, .object(["type": .string("list")]))
        let allKeys = (r["keys"].asArray ?? []).map { $0["key"].asString ?? "" }
        #expect(allKeys == ["nodes.a", "nodes.b", "recents.x"])
    }

    @Test func setRejectsOversizeValue() async throws {
        let (kernel, kv) = await bootKV(available())
        let big = String(repeating: "x", count: 1_048_577)  // > 1 MB per-value limit
        let r = await kernel.send(
            kv,
            .object(["type": .string("set"), "key": .string("nodes.big"), "value": .string(big)]))
        #expect(r["set"].asBool == nil)
        #expect(r["error"].asString?.contains("per-value") == true)
        #expect(r["hint"].asString?.contains("CloudKit") == true)
    }

    @Test func reflectShapeWhenAvailable() async throws {
        let (kernel, kv) = await bootKV(available())
        _ = await kernel.send(
            kv,
            .object(["type": .string("set"), "key": .string("nodes.a"), "value": .string("v")]))
        let r = await kernel.send(kv, .object(["type": .string("reflect")]))
        #expect(r["backing"].asString == "apple_kvs")
        #expect(r["available"].asBool == true)
        #expect(r["synced"].asBool == true)
        #expect(r["key_count"].asInt == 1)
        #expect((r["namespaces"].asArray ?? []).map { $0.asString } == ["nodes"])
        #expect(r["usage"]["keys"].asInt == 1)
        #expect(r["limits"]["keys"].asInt == 1024)
        #expect(r["verbs"]["set"].asString != nil)
        #expect(r["verbs"]["watch"].asString != nil)
    }

    @Test func gatedWhenUnavailable_neverReadsCache() async throws {
        // Pre-seed the backing, then force unavailable: data verbs must gate
        // and crucially get/list must NOT surface the cached value.
        let mem = MemoryKVSBacking()
        mem.set("\"cached\"", forKey: "nodes.a")
        let bundle = AppleKVSBundle(
            store: mem,
            manager: AppleKVSManager(availabilityOverride: { (false, "test: offline") }))
        let (kernel, kv) = await bootKV(bundle)

        for verb in ["read", "list", "set", "delete"] {
            let r = await kernel.send(
                kv, .object(["type": .string(verb), "key": .string("nodes.a")]))
            #expect(r["unavailable"].asBool == true, "\(verb) should be gated")
            #expect(r["reason"].asString == "test: offline")
            #expect(r["value"].asString == nil)  // no cached leak
        }
        let rr = await kernel.send(kv, .object(["type": .string("reflect")]))
        #expect(rr["available"].asBool == false)
        #expect(rr["key_count"].asInt == 0)  // unavailable → don't trust the cache
    }

    #if canImport(Darwin)
        @Test func watchEmitsChangedOnExternalUpdate() async throws {
            let (kernel, kv) = await bootKV(available())
            _ = await kernel.send(kv, .object(["type": .string("boot")]))

            let watcher = AgentId("test_watcher")
            kernel.watch(src: kv, watcher: watcher)
            let stream = kernel.ensureInbox(watcher)

            // Simulate a cross-device write surfacing via iCloud KVS.
            NotificationCenter.default.post(
                name: NSUbiquitousKeyValueStore.didChangeExternallyNotification,
                object: nil,
                userInfo: [NSUbiquitousKeyValueStoreChangedKeysKey: ["nodes.x"]])

            let ev = await firstEvent(stream)
            #expect(ev?["type"].asString == "changed")
            #expect((ev?["keys"].asArray ?? []).map { $0.asString } == ["nodes.x"])

            _ = await kernel.send(kv, .object(["type": .string("shutdown")]))
        }
    #endif
}

/// Await the first event on a stream, or nil after a short timeout.
private func firstEvent(_ stream: AsyncStream<JSON>) async -> JSON? {
    await withTaskGroup(of: JSON?.self) { group in
        group.addTask {
            for await ev in stream { return ev }
            return nil
        }
        group.addTask {
            try? await Task.sleep(nanoseconds: 3_000_000_000)
            return nil
        }
        let first = await group.next() ?? nil
        group.cancelAll()
        return first
    }
}
