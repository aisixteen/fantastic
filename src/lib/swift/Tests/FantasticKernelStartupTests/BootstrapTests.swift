// 8H: daemon bootstrap + flock tests.

import Darwin
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@Suite("WorkdirLock")
struct WorkdirLockTests {
    @Test func acquireAndReleaseWritesPid() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let lock = WorkdirLock(workdir: tmp)
        try lock.acquire()
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.json")
        let data = try Data(contentsOf: lockPath)
        let parsed = try JSON.parse(data)
        #expect(parsed["pid"].asInt == Int64(getpid()))
        lock.release()
    }

    @Test func acquireFailsWhenAlreadyHeld() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let first = WorkdirLock(workdir: tmp)
        try first.acquire()
        defer { first.release() }

        let second = WorkdirLock(workdir: tmp)
        #expect(throws: WorkdirLock.LockError.self) {
            try second.acquire()
        }
    }

    @Test func staleLockIsReclaimed() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        // Write a lock file pointing at a definitely-dead pid (0 is
        // never a real user-space pid).
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.json")
        try FileManager.default.createDirectory(
            at: lockPath.deletingLastPathComponent(),
            withIntermediateDirectories: true)
        let stale = #"{"pid": 0}"#
        try stale.write(to: lockPath, atomically: true, encoding: .utf8)

        // Should acquire cleanly even though a lock.json already exists.
        let lock = WorkdirLock(workdir: tmp)
        try lock.acquire()
        let data = try Data(contentsOf: lockPath)
        let parsed = try JSON.parse(data)
        #expect(parsed["pid"].asInt == Int64(getpid()))
        lock.release()
    }

    /// C50: python's `json.dumps({"pid": N})` bytes — WITH the space after
    /// `:` (`', '`/`': '` are `json.dumps`'s default separators even
    /// compact), never this runtime's serde-shaped `serialize()`.
    @Test func testLockBytesArePythonJsonDumps() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let lock = WorkdirLock(workdir: tmp)
        try lock.acquire()
        defer { lock.release() }
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.json")
        let bytes = try String(contentsOf: lockPath, encoding: .utf8)
        #expect(bytes == "{\"pid\": \(getpid())}", "\(bytes)")
    }

    /// C82: a live recorded pid refuses acquisition even when NOTHING holds
    /// this file's `flock` — the shape a python or rust daemon's lock is
    /// always in, since neither takes one. The previous swift ordering
    /// (flock checked first) treated an uncontested flock as free rein to
    /// overwrite a live foreign pid.
    @Test func acquireRefusesALiveRecordedPidWithoutFlock() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.json")
        try FileManager.default.createDirectory(
            at: lockPath.deletingLastPathComponent(), withIntermediateDirectories: true)
        // getpid() is a genuinely live pid (this very test process) and the
        // file is written with NO flock held on it — exactly a python/rust
        // daemon's own lock shape.
        try #"{"pid": \#(getpid())}"#.write(to: lockPath, atomically: true, encoding: .utf8)
        let lock = WorkdirLock(workdir: tmp)
        #expect(throws: WorkdirLock.LockError.self) {
            try lock.acquire()
        }
    }

    /// C82: `release()` removes the FILE only when it still names OUR pid —
    /// a second kernel that reclaimed the (now-stale) lock in the meantime
    /// must not have its live lock deleted out from under it by the first
    /// kernel's own (delayed) release.
    @Test func releaseKeepsAForeignLock() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let lock = WorkdirLock(workdir: tmp)
        try lock.acquire()
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.json")
        // Simulate a second kernel having reclaimed the file after our
        // in-memory `fileDescriptor` was captured: a DIFFERENT recorded pid
        // (release only checks identity against OUR pid, never liveness, so
        // a fabricated foreign number exercises the same branch a genuine
        // second kernel would).
        let foreignPid = getpid() &+ 1
        try #"{"pid": \#(foreignPid)}"#.write(to: lockPath, atomically: true, encoding: .utf8)
        lock.release()
        #expect(
            FileManager.default.fileExists(atPath: lockPath.path),
            "a foreign lock must survive release")
        let stillForeign = try JSON.parse(Data(contentsOf: lockPath))
        #expect(
            stillForeign["pid"].asInt == Int64(foreignPid),
            "release must not touch the foreign content")
    }
}

@Suite("Bootstrap")
struct BootstrapTests {
    @Test func daemonBootstrapAcquiresLock() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let (kernel, lock) = try await Bootstrap.daemon(workdir: tmp.path)
        defer { lock.release() }

        // Lock file should exist + name our pid.
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.json")
        #expect(FileManager.default.fileExists(atPath: lockPath.path))

        // Core agent registered.
        let listed = await kernel.send([
            Item(AgentId("kernel_state"), .object(["type": .string("list_agents")]))
        ])[0].result
        let ids = (listed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("kernel_state"))
    }

    @Test func daemonRefusesIfAlreadyLocked() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let (_, lock) = try await Bootstrap.daemon(workdir: tmp.path)
        defer { lock.release() }

        do {
            let _ = try await Bootstrap.daemon(workdir: tmp.path)
            Issue.record("expected daemon to refuse second acquire")
        } catch {
            // Expected.
        }
    }

    @Test func oneShotDoesNotAcquireLock() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        // Daemon holds the lock.
        let (_, lock) = try await Bootstrap.daemon(workdir: tmp.path)
        defer { lock.release() }

        // One-shot should succeed even though the daemon owns the lock.
        let probe = try await Bootstrap.oneShot(workdir: tmp.path)
        let listed = await probe.send([
            Item(AgentId("kernel_state"), .object(["type": .string("list_agents")]))
        ])[0].result
        let ids = (listed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("kernel_state"))
    }
}

/// C52 + C53 + C61 — the hydration walk is python's `read_tree`: the root
/// record names the children container, the DISK POSITION sets every
/// `parent_id`, entries are visited sorted, a corrupt record takes its subtree
/// with it silently, and the file's key order survives into the record.
@Suite("hydration walk = python read_tree")
struct HydrationWalkTests {

    private func seed(_ dir: URL, _ rel: String, _ body: String) throws {
        let f = dir.appendingPathComponent(rel)
        try FileManager.default.createDirectory(
            at: f.deletingLastPathComponent(), withIntermediateDirectories: true)
        try body.write(to: f, atomically: true, encoding: .utf8)
    }

    @Test func testHydrationTakesParentFromDiskPosition() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent(".fantastic")
        try seed(store, "agent.json", #"{"id": "kernel_state", "parent_id": null}"#)
        // A STALE parent_id on disk: the position wins, not the field.
        try seed(store, "agents/a/agent.json", #"{"id": "a", "parent_id": "bogus"}"#)
        try seed(
            store, "agents/a/agents/b/agent.json", #"{"id": "b", "parent_id": "kernel_state"}"#)

        let recs = Persistence.readTree(rootDir: store)
        #expect(recs.map(\.id) == ["kernel_state", "a", "b"])
        #expect(recs.map(\.parentId) == [nil, "kernel_state", "a"])
    }

    @Test func testCorruptRecordSkipsItsSubtree() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent(".fantastic")
        try seed(store, "agent.json", #"{"id": "kernel_state", "parent_id": null}"#)
        try seed(store, "agents/bad/agent.json", "{ this is not json")
        try seed(store, "agents/bad/agents/kid/agent.json", #"{"id": "kid"}"#)
        // A dir with NO agent.json is likewise skipped and not descended.
        try seed(store, "agents/empty/agents/ghost/agent.json", #"{"id": "ghost"}"#)
        try seed(store, "agents/good/agent.json", #"{"id": "good"}"#)

        let recs = Persistence.readTree(rootDir: store)
        #expect(recs.map(\.id) == ["kernel_state", "good"])
    }

    @Test func testHydrationWalksSortedOrder() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent(".fantastic")
        try seed(store, "agent.json", #"{"id": "kernel_state", "parent_id": null}"#)
        for id in ["zeta", "alpha", "mid"] {
            try seed(store, "agents/\(id)/agent.json", #"{"id": "\#(id)"}"#)
        }
        let recs = Persistence.readTree(rootDir: store)
        #expect(recs.map(\.id) == ["kernel_state", "alpha", "mid", "zeta"])
    }

    /// C52 — the container's NAME is declared config on the root record.
    @Test func testDeclaredChildrenDirRoundTrips() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent(".fantastic")
        try seed(
            store, "agent.json",
            #"{"id": "kernel_state", "parent_id": null, "children_dir": "host_agents"}"#)
        try seed(store, "host_agents/a/agent.json", #"{"id": "a"}"#)
        try seed(store, "host_agents/a/host_agents/b/agent.json", #"{"id": "b"}"#)
        // A tree under the DEFAULT name is invisible once another is declared.
        try seed(store, "agents/never/agent.json", #"{"id": "never"}"#)

        let recs = Persistence.readTree(rootDir: store)
        #expect(recs.map(\.id) == ["kernel_state", "a", "b"])

        // `.disk` takes the WORKDIR; the kernel appends `.fantastic` itself.
        let kernel = Kernel(storage: .disk(tmp), bundles: BundleRegistry())
        try kernel.load(KernelState(agents: recs))
        #expect(kernel.childrenDir == "host_agents")
        #expect(
            kernel.agent(AgentId("b"))?.rootPath.path
                == store.appendingPathComponent("host_agents/a/host_agents/b").path)
    }

    /// C61 — the record's meta (and every nested map) keeps the file's order.
    @Test func hydratedChildRecordKeepsMetaAndNestedKeyOrder() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent(".fantastic")
        try seed(store, "agent.json", #"{"id": "kernel_state", "parent_id": null}"#)
        try seed(
            store, "agents/web/agent.json",
            #"""
            {"id": "web", "handler_module": "web.tools", "port": 8080,
             "can_call": {"$self": "*", "$parent": "*", "rest": ["reflect"], "ws": "*"},
             "display_name": "Web"}
            """#)
        let recs = Persistence.readTree(rootDir: store)
        let web = try #require(recs.first { $0.id == "web" })
        #expect(web.meta.keys.elements == ["port", "can_call", "display_name"])
        #expect(
            web.meta["can_call"]?.asObject?.keys.elements == ["$self", "$parent", "rest", "ws"])
    }

    /// C53 — a record whose bundle is not installed HERE is skipped with
    /// python's exact stderr line, and it takes its subtree with it.
    @Test func weakLoadSkipsUnknownBundleWithPythonLine() async throws {
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        let state = KernelState(agents: [
            AgentRecord(id: "kernel_state", handlerModule: nil, parentId: nil),
            AgentRecord(id: "ts_thing", handlerModule: "browser.tools", parentId: "kernel_state"),
            AgentRecord(id: "under_it", handlerModule: nil, parentId: "ts_thing"),
        ])
        let lines = try await RuntimeLog.capture {
            try kernel.load(state)
        }
        #expect(
            lines == [
                "[kernel] skipping agent ts_thing: bundle browser.tools not installed in "
                    + "this runtime\n"
            ], "\(lines)")
        #expect(kernel.agent(AgentId("kernel_state")) != nil)
        #expect(kernel.agent(AgentId("ts_thing")) == nil)
        #expect(kernel.agent(AgentId("under_it")) == nil, "the subtree goes with it")
    }
}

/// C56 + C14 + RP-11 — what a boot does, and what it does NOT.
@Suite("root record: reseed, boot order, teardown")
struct RootBootTests {
    private func seedChild(_ tmp: URL, _ id: String) throws {
        let dir = tmp.appendingPathComponent(".fantastic/agents/\(id)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let rec = #"{"id": "\#(id)", "handler_module": null, "parent_id": null}"#
        try rec.write(
            to: dir.appendingPathComponent("agent.json"), atomically: true, encoding: .utf8)
    }

    /// C56 — python's `read_tree` returns `[]` when the root `agent.json` is
    /// MISSING **or not JSON**, and the bootstrap then seeds a fresh root,
    /// overwrites the file, and hydrates NO children on that boot. Swift used to
    /// overwrite the corrupt root and hydrate the children anyway.
    @Test func testCorruptRootRecordReseedsAndKeepsChildren() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let dot = tmp.appendingPathComponent(".fantastic")
        try FileManager.default.createDirectory(at: dot, withIntermediateDirectories: true)
        try "{not json".write(
            to: dot.appendingPathComponent("agent.json"), atomically: true, encoding: .utf8)
        try seedChild(tmp, "orphan")

        let kernel = try await startKernel(workdir: tmp.path)
        #expect(kernel.root?.id.value == "kernel_state")
        #expect(kernel.root?.handlerModule == "kernel_state.tools")
        #expect(kernel.agent(AgentId("orphan")) != nil, "children survive a reseed (python 2026-09-20)")
        // The file was overwritten with the CONSTRUCTED root: the grant baseline
        // in its order, and no guid yet (the loader's boot mints that).
        let parsed = try JSON.parse(try Data(contentsOf: dot.appendingPathComponent("agent.json")))
        #expect(parsed["id"].asString == "kernel_state")
        #expect(parsed["handler_module"].asString == "kernel_state.tools")
        #expect(
            parsed.asObject.map { Array($0.keys) }
                == ["id", "handler_module", "can_call", "callable_by"], "\(parsed.serialize())")
    }

    /// The readme + docs tree is seeded ONLY on the SEED boot, as python does
    /// (`_bootstrap` seeds through `write_record` under `if seeded`). Swift
    /// seeded on EVERY invocation, so a doc the operator deleted came back on
    /// the next command.
    @Test func docsAreSeededOnlyWhenTheRootIsSeeded() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let fm = FileManager.default
        // First boot: a virgin dir seeds the root record, the readme and docs.
        _ = try await startKernel(workdir: tmp.path)
        let dot = tmp.appendingPathComponent(".fantastic")
        let memory = dot.appendingPathComponent("docs/memory.md")
        let frontend = dot.appendingPathComponent("docs/frontend")
        #expect(fm.fileExists(atPath: memory.path), "the seed boot writes the doc tree")
        #expect(fm.fileExists(atPath: frontend.path))
        // The operator removes part of the tree; a second boot reads the root
        // record, so nothing is re-seeded.
        try fm.removeItem(at: memory)
        try fm.removeItem(at: frontend)
        _ = try await startKernel(workdir: tmp.path)
        #expect(!fm.fileExists(atPath: memory.path), "a deleted doc stays deleted")
        #expect(!fm.fileExists(atPath: frontend.path))
    }

    /// A MISSING root record is the same path — a virgin dir.
    @Test func testMissingRootRecordSeedsAndKeepsChildren() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        try seedChild(tmp, "orphan")
        let kernel = try await startKernel(workdir: tmp.path)
        #expect(kernel.agent(AgentId("orphan")) != nil)
        #expect(Set(kernel.allAgents().map { $0.id.value }) == ["kernel_state", "orphan"])
    }

    /// C12 — a PYTHON-SHAPED root record hydrates verbatim: its id and its
    /// handler_module are the tree's, not a default the port substitutes.
    @Test func testPythonShapedRootRecordHydrates() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let dot = tmp.appendingPathComponent(".fantastic")
        try FileManager.default.createDirectory(at: dot, withIntermediateDirectories: true)
        let rec = """
            {
              "id": "kernel_state",
              "handler_module": "kernel_state.tools",
              "can_call": {"$self": "*", "$parent": "*", "$children": "*"},
              "callable_by": {"$self": "*", "$parent": "*", "$children": "*"},
              "guid": "0123456789abcdef0123456789abcdef"
            }
            """
        try rec.write(
            to: dot.appendingPathComponent("agent.json"), atomically: true, encoding: .utf8)
        let kernel = try await startKernel(workdir: tmp.path)
        #expect(kernel.root?.id.value == "kernel_state")
        #expect(kernel.root?.handlerModule == "kernel_state.tools")
        #expect(
            kernel.root?.metaValue(forKey: "guid")?.asString
                == "0123456789abcdef0123456789abcdef")
    }

    /// C14/RP-11 — the daemon's boot pass walks the REGISTRATION-ordered index,
    /// root FIRST, and mints nothing else: no `list_agents` send to the root and
    /// no post-boot `reflect` on web, both of which landed as ledger executions
    /// and call rows before the pump even existed.
    @Test func testDaemonBootMintsOnlyBootExecutions() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let (kernel, lock) = try await Bootstrap.daemon(workdir: tmp.path)
        defer { lock.release() }
        for agent in kernel.allAgents() {
            _ = await kernel.send([Item(agent.id, .object(["type": .string("boot")]))])[0]
        }
        let verbs = kernel.rows.forAgent(AgentId("kernel_state"))
            .filter { $0["kind"]?.asString == "call" }
            .compactMap { $0["verb"]?.asString }
        #expect(verbs == ["boot"], "the boot pass minted \(verbs)")
        await kernel.shutdown()
    }

    /// C85 — `shutdownRequested()` resolves on CANCELLATION as well as on the
    /// stop signal. It parked on a bare continuation, so the loser of the
    /// daemon's race was never resumed, `cancelAll()` waited forever, and the
    /// process had to be SIGKILLed with `lock.json` left behind.
    @Test func awaitStopReturnsWhenCancelled() async throws {
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        let root = Agent(id: AgentId(ROOT_ID), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let returned = await withTaskGroup(of: Bool.self) { group in
            group.addTask {
                _ = await Bootstrap.awaitStop(kernel: kernel)
                return true
            }
            group.addTask {
                // The OTHER leg of the daemon's race finishes first.
                try? await Task.sleep(nanoseconds: 50_000_000)
                return false
            }
            let first = await group.next() ?? false
            group.cancelAll()
            // The cancelled `awaitStop` must now RETURN — this await is the
            // test: it hung forever before.
            await group.waitForAll()
            return first
        }
        #expect(returned == false, "the signal leg should not have fired")
    }
}

private func makeTempDir() -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(
        "fantastic-bootstrap-test-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}
