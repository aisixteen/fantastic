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
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.yaml")
        let data = try Data(contentsOf: lockPath)
        let parsed = try YamlIO.load(String(decoding: data, as: UTF8.self))
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
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.yaml")
        try FileManager.default.createDirectory(
            at: lockPath.deletingLastPathComponent(),
            withIntermediateDirectories: true)
        let stale = #"{"pid": 0}"#
        try stale.write(to: lockPath, atomically: true, encoding: .utf8)

        // Should acquire cleanly even though a lock.yaml already exists.
        let lock = WorkdirLock(workdir: tmp)
        try lock.acquire()
        let data = try Data(contentsOf: lockPath)
        let parsed = try YamlIO.load(String(decoding: data, as: UTF8.self))
        #expect(parsed["pid"].asInt == Int64(getpid()))
        lock.release()
    }

    /// Python's `yaml.dump({"pid": N})` bytes — `pid: N` + newline.
    @Test func testLockBytesArePythonYamlDump() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let lock = WorkdirLock(workdir: tmp)
        try lock.acquire()
        defer { lock.release() }
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.yaml")
        let bytes = try String(contentsOf: lockPath, encoding: .utf8)
        #expect(bytes == "pid: \(getpid())\n", "\(bytes)")
    }

    /// The pid in the file is for the message, never the decision: a file
    /// naming a LIVE pid (this very test process) that holds no flock — its
    /// holder gone, the pid reused — is taken over. Python, rust and swift all
    /// decide by the flock alone.
    @Test func aLivePidWithoutTheFlockIsTakenOver() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.yaml")
        try FileManager.default.createDirectory(
            at: lockPath.deletingLastPathComponent(), withIntermediateDirectories: true)
        try #"{"pid": \#(getppid())}"#.write(to: lockPath, atomically: true, encoding: .utf8)
        let lock = WorkdirLock(workdir: tmp)
        try lock.acquire()
        defer { lock.release() }
        let bytes = try String(contentsOf: lockPath, encoding: .utf8)
        #expect(bytes == "pid: \(getpid())\n", "\(bytes)")
    }

    /// The refusal names the holder's pid, and leaves no staged file behind.
    @Test func refusalNamesTheHolderAndLeavesNoStagedFile() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let first = WorkdirLock(workdir: tmp)
        try first.acquire()
        defer { first.release() }
        do {
            try WorkdirLock(workdir: tmp).acquire()
            Issue.record("a second acquisition must be refused")
        } catch WorkdirLock.LockError.alreadyRunning(let pid, _) {
            #expect(pid == getpid())
        }
        let names = try FileManager.default.contentsOfDirectory(
            atPath: tmp.appendingPathComponent(".fantastic").path)
        #expect(names == ["lock.yaml"], "\(names)")
    }

    @Test func releaseLetsTheNextAcquireIn() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let first = WorkdirLock(workdir: tmp)
        try first.acquire()
        first.release()
        let second = WorkdirLock(workdir: tmp)
        try second.acquire()
        second.release()
    }

    /// A read-then-write lock would let two acquisitions racing in one dir
    /// BOTH take it. Each racer here opens its own
    /// descriptor, and flock excludes descriptors of one process as it
    /// excludes processes — so every round, on a fresh dir and over a dead
    /// holder's lock alike, exactly one wins.
    @Test(arguments: [false, true])
    func twoRacingAcquisitionsExactlyOneWins(overDeadLock: Bool) throws {
        for _ in 0..<20 {
            let tmp = makeTempDir()
            defer { try? FileManager.default.removeItem(at: tmp) }
            if overDeadLock {
                let lockPath = tmp.appendingPathComponent(".fantastic/lock.yaml")
                try FileManager.default.createDirectory(
                    at: lockPath.deletingLastPathComponent(), withIntermediateDirectories: true)
                try "pid: 2147483646\n".write(to: lockPath, atomically: true, encoding: .utf8)
            }
            let locks = [WorkdirLock(workdir: tmp), WorkdirLock(workdir: tmp)]
            let gate = NSCondition()
            nonisolated(unsafe) var arrived = 0
            nonisolated(unsafe) var won = [false, false]
            DispatchQueue.concurrentPerform(iterations: 2) { i in
                gate.lock()
                arrived += 1
                gate.broadcast()
                while arrived < 2 { gate.wait() }
                gate.unlock()
                let ok = (try? locks[i].acquire()) != nil
                gate.lock()
                won[i] = ok
                gate.unlock()
            }
            #expect(won.filter { $0 }.count == 1, "\(won)")
            locks.forEach { $0.release() }
        }
    }

    /// `release()` removes the FILE only when it still names OUR pid —
    /// a second kernel that reclaimed the (now-stale) lock in the meantime
    /// must not have its live lock deleted out from under it by the first
    /// kernel's own (delayed) release.
    @Test func releaseKeepsAForeignLock() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let lock = WorkdirLock(workdir: tmp)
        try lock.acquire()
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.yaml")
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
        let stillForeign = try YamlIO.load(String(decoding: Data(contentsOf: lockPath), as: UTF8.self))
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
        let lockPath = tmp.appendingPathComponent(".fantastic/lock.yaml")
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

    /// python reads the tree ONLY from the `agent.yaml` records: a
    /// `.fantastic/state.json` left in the workdir is never loaded over it.
    @Test func aStateJsonIsNeverLoadedOverTheTree() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let dot = tmp.appendingPathComponent(".fantastic")
        try FileManager.default.createDirectory(at: dot, withIntermediateDirectories: true)
        let snapshot =
            #"{"version":1,"records":[{"id":"kernel_state"},"#
            + #"{"id":"ghost","parent_id":"kernel_state"}]}"#
        try snapshot.write(
            to: dot.appendingPathComponent("state.json"), atomically: true, encoding: .utf8)
        let (kernel, lock) = try await Bootstrap.daemon(workdir: tmp.path)
        defer { lock.release() }
        #expect(kernel.agent(AgentId("ghost")) == nil, "state.json was loaded over the tree")
        let probe = try await Bootstrap.oneShot(workdir: tmp.path)
        #expect(probe.agent(AgentId("ghost")) == nil, "state.json was loaded over the tree")
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

/// The hydration walk is python's `read_tree`: the root
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
        try seed(store, "agent.yaml", #"{"id": "kernel_state", "parent_id": null}"#)
        // A STALE parent_id on disk: the position wins, not the field.
        try seed(store, "agents/a/agent.yaml", #"{"id": "a", "parent_id": "bogus"}"#)
        try seed(
            store, "agents/a/agents/b/agent.yaml", #"{"id": "b", "parent_id": "kernel_state"}"#)

        let recs = try Persistence.readTree(rootDir: store)
        #expect(recs.map(\.id) == ["kernel_state", "a", "b"])
        #expect(recs.map(\.parentId) == [nil, "kernel_state", "a"])
    }

    @Test func testCorruptRecordSkipsItsSubtree() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent(".fantastic")
        try seed(store, "agent.yaml", #"{"id": "kernel_state", "parent_id": null}"#)
        try seed(store, "agents/bad/agent.yaml", "{ this is not json")
        try seed(store, "agents/bad/agents/kid/agent.yaml", #"{"id": "kid"}"#)
        // A dir with NO agent.yaml is likewise skipped and not descended.
        try seed(store, "agents/empty/agents/ghost/agent.yaml", #"{"id": "ghost"}"#)
        try seed(store, "agents/good/agent.yaml", #"{"id": "good"}"#)

        let recs = try Persistence.readTree(rootDir: store)
        #expect(recs.map(\.id) == ["kernel_state", "good"])
    }

    @Test func testHydrationWalksSortedOrder() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent(".fantastic")
        try seed(store, "agent.yaml", #"{"id": "kernel_state", "parent_id": null}"#)
        for id in ["zeta", "alpha", "mid"] {
            try seed(store, "agents/\(id)/agent.yaml", #"{"id": "\#(id)"}"#)
        }
        let recs = try Persistence.readTree(rootDir: store)
        #expect(recs.map(\.id) == ["kernel_state", "alpha", "mid", "zeta"])
    }

    /// The container's NAME is declared config on the root record.
    @Test func testDeclaredChildrenDirRoundTrips() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent(".fantastic")
        try seed(
            store, "agent.yaml",
            #"{"id": "kernel_state", "parent_id": null, "children_dir": "host_agents"}"#)
        try seed(store, "host_agents/a/agent.yaml", #"{"id": "a"}"#)
        try seed(store, "host_agents/a/host_agents/b/agent.yaml", #"{"id": "b"}"#)
        // A tree under the DEFAULT name is invisible once another is declared.
        try seed(store, "agents/never/agent.yaml", #"{"id": "never"}"#)

        let recs = try Persistence.readTree(rootDir: store)
        #expect(recs.map(\.id) == ["kernel_state", "a", "b"])

        // `.disk` takes the WORKDIR; the kernel appends `.fantastic` itself.
        let kernel = Kernel(storage: .disk(tmp), bundles: BundleRegistry())
        try kernel.load(KernelState(records: recs))
        #expect(kernel.childrenDir == "host_agents")
        #expect(
            kernel.agent(AgentId("b"))?.rootPath.path
                == store.appendingPathComponent("host_agents/a/host_agents/b").path)
    }

    /// The record's meta (and every nested map) keeps the file's order.
    @Test func hydratedChildRecordKeepsMetaAndNestedKeyOrder() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent(".fantastic")
        try seed(store, "agent.yaml", #"{"id": "kernel_state", "parent_id": null}"#)
        try seed(
            store, "agents/web/agent.yaml",
            #"""
            {"id": "web", "handler_module": "web.tools", "port": 8080,
             "can_call": {"$self": "*", "$parent": "*", "rest": ["reflect"], "ws": "*"},
             "display_name": "Web"}
            """#)
        let recs = try Persistence.readTree(rootDir: store)
        let web = try #require(recs.first { $0.id == "web" })
        #expect(web.meta.keys.elements == ["port", "can_call", "display_name"])
        #expect(
            web.meta["can_call"]?.asObject?.keys.elements == ["$self", "$parent", "rest", "ws"])
    }

    /// A record whose bundle is not installed HERE is skipped with
    /// python's exact stderr line, and it takes its subtree with it.
    @Test func weakLoadSkipsUnknownBundleWithPythonLine() async throws {
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        let state = KernelState(records: [
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

    /// Python's cold walk opens only through the `fs` clamp, so a
    /// `children_dir` that resolves outside `.fantastic` raises — the boot
    /// fails — never joined raw to hydrate another directory.
    @Test func coldWalkRefusesAnEscapingChildrenDir() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let work = tmp.appendingPathComponent("work")
        try seed(work, ".fantastic/agent.yaml", "id: kernel_state\nchildren_dir: ../sibling\n")
        try seed(work, "sibling/probe_a/agent.yaml", "id: probe_a\n")
        #expect(throws: PyError("ValueError", "fs: path '../sibling' escapes root")) {
            try Persistence.readTree(rootDir: work.appendingPathComponent(".fantastic"))
        }
    }

    @Test func coldWalkRefusesAnOutwardSymlinkedEntry() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent("work/.fantastic")
        try seed(store, "agent.yaml", "id: kernel_state\n")
        try seed(tmp, "outside/agent.yaml", "id: probe_out\n")
        try FileManager.default.createDirectory(
            at: store.appendingPathComponent("agents"), withIntermediateDirectories: true)
        try FileManager.default.createSymbolicLink(
            at: store.appendingPathComponent("agents/evil"),
            withDestinationURL: tmp.appendingPathComponent("outside"))
        #expect(
            throws: PyError("ValueError", "fs: path 'agents/evil/agent.yaml' escapes root")
        ) {
            try Persistence.readTree(rootDir: store)
        }
    }

    @Test func coldWalkFollowsAnInwardSymlinkedEntry() throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let store = tmp.appendingPathComponent("work/.fantastic")
        try seed(store, "agent.yaml", "id: kernel_state\n")
        try seed(store, "stash/a/agent.yaml", "id: a\n")
        try FileManager.default.createDirectory(
            at: store.appendingPathComponent("agents"), withIntermediateDirectories: true)
        try FileManager.default.createSymbolicLink(
            at: store.appendingPathComponent("agents/a"),
            withDestinationURL: store.appendingPathComponent("stash/a"))
        let recs = try Persistence.readTree(rootDir: store)
        #expect(recs.map(\.id) == ["kernel_state", "a"])
        #expect(recs.last?.parentId == "kernel_state")
    }
}

/// What a boot does, and what it does NOT.
@Suite("root record: reseed, boot order, teardown")
struct RootBootTests {
    private func seedChild(_ tmp: URL, _ id: String) throws {
        let dir = tmp.appendingPathComponent(".fantastic/agents/\(id)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let rec = #"{"id": "\#(id)", "handler_module": null, "parent_id": null}"#
        try rec.write(
            to: dir.appendingPathComponent("agent.yaml"), atomically: true, encoding: .utf8)
    }

    /// A root record that is THERE but does not parse is never written over: the
    /// start refuses and the file stays as it was (python `main._bootstrap`).
    @Test func anUnreadableRootRecordRefusesTheStartAndIsLeftAlone() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let dot = tmp.appendingPathComponent(".fantastic")
        try FileManager.default.createDirectory(at: dot, withIntermediateDirectories: true)
        try "{not json".write(
            to: dot.appendingPathComponent("agent.yaml"), atomically: true, encoding: .utf8)
        try seedChild(tmp, "orphan")
        await #expect(throws: (any Error).self) { _ = try await startKernel(workdir: tmp.path) }
        #expect(
            try String(contentsOf: dot.appendingPathComponent("agent.yaml"), encoding: .utf8)
                == "{not json")
    }

    /// A MISSING root record reseeds the CONSTRUCTED root (the grant baseline in
    /// its order, no guid yet — the loader's boot mints that) and keeps the
    /// children; the record lands only when the lock is held (`takeSeed`).
    @Test func aMissingRootRecordReseedsAndKeepsChildren() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let dot = tmp.appendingPathComponent(".fantastic")
        try seedChild(tmp, "orphan")
        let kernel = try await startKernel(workdir: tmp.path)
        #expect(kernel.root?.id.value == "kernel_state")
        #expect(kernel.agent(AgentId("orphan")) != nil, "children survive a reseed")
        #expect(!FileManager.default.fileExists(atPath: dot.appendingPathComponent("agent.yaml").path))
        try kernel.takeSeed()
        let parsed = try YamlIO.load(String(decoding: try Data(contentsOf: dot.appendingPathComponent("agent.yaml")), as: UTF8.self))
        #expect(
            parsed.asObject.map { Array($0.keys) }
                == ["id", "handler_module", "can_call", "callable_by"], "\(parsed.serialize())")
    }

    /// No boot writes a readme or `docs/` into `.fantastic` — the seed boot
    /// writes the root record alone; the manual is served from the bundle.
    @Test func noBootWritesTheReadmeOrDocs() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let fm = FileManager.default
        let dot = tmp.appendingPathComponent(".fantastic")
        for _ in 0..<2 {
            try await startKernel(workdir: tmp.path).takeSeed()
            #expect(fm.fileExists(atPath: dot.appendingPathComponent("agent.yaml").path))
            #expect(!fm.fileExists(atPath: dot.appendingPathComponent("readme.md").path))
            #expect(!fm.fileExists(atPath: dot.appendingPathComponent("docs").path))
        }
    }

    /// A `.fantastic` that cannot be made, or a root record that cannot be
    /// written, FAILS the start (the seed under the lock), as python's seed raises. Swallowed, the
    /// kernel would boot "fine" with an unpersisted root and mint a new guid every
    /// start, breaking peers pinned to it.
    @Test func aFailedColdSeedFailsTheStart() async throws {
        let fm = FileManager.default
        let blockedDir = makeTempDir()
        defer { try? fm.removeItem(at: blockedDir) }
        try "a FILE where .fantastic goes".write(
            to: blockedDir.appendingPathComponent(".fantastic"), atomically: true, encoding: .utf8)
        await #expect(throws: (any Error).self) {
            try await startKernel(workdir: blockedDir.path).takeSeed()
        }

        let blockedRecord = makeTempDir()
        defer { try? fm.removeItem(at: blockedRecord) }
        try fm.createDirectory(
            at: blockedRecord.appendingPathComponent(".fantastic/agent.yaml"),
            withIntermediateDirectories: true)
        await #expect(throws: (any Error).self) {
            _ = try await startKernel(workdir: blockedRecord.path)
        }
    }

    /// The boot reads the root record through the `fs` clamp, as
    /// python's `read_tree` does, so a root `agent.yaml` linked outside
    /// `.fantastic` refuses the start with `fs`'s text — and NOTHING is reseeded
    /// over the link (read raw, a dangling link would read as "no record" and the
    /// seed would write a fresh root over it).
    @Test func anOutwardSymlinkedRootRecordRefusesTheStartAndIsNotReseeded() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let fm = FileManager.default
        let work = tmp.appendingPathComponent("work")
        let dot = work.appendingPathComponent(".fantastic")
        try fm.createDirectory(at: dot, withIntermediateDirectories: true)
        let link = dot.appendingPathComponent("agent.yaml")
        let outside = tmp.appendingPathComponent("outside.yaml")
        try fm.createSymbolicLink(at: link, withDestinationURL: outside)
        let escapes = PyError("ValueError", "fs: path 'agent.yaml' escapes root")

        // dangling: the target does not exist
        await #expect(throws: escapes) { _ = try await startKernel(workdir: work.path) }
        #expect(try fm.destinationOfSymbolicLink(atPath: link.path) == outside.path)
        #expect(!fm.fileExists(atPath: outside.path), "nothing was written through the link")

        // a mapping outside is refused the same way, and left as it was
        try "id: kernel_state\n".write(to: outside, atomically: true, encoding: .utf8)
        await #expect(throws: escapes) { _ = try await startKernel(workdir: work.path) }
        #expect(try fm.destinationOfSymbolicLink(atPath: link.path) == outside.path)
        #expect(try String(contentsOf: outside, encoding: .utf8) == "id: kernel_state\n")
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

    /// A PYTHON-SHAPED root record hydrates verbatim: its id and its
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
              "can_call": {"$self": "*", "$parent": "*", "$subtree": "*"},
              "callable_by": {"$self": "*", "$parent": "*", "$subtree": "*"},
              "guid": "0123456789abcdef0123456789abcdef"
            }
            """
        try rec.write(
            to: dot.appendingPathComponent("agent.yaml"), atomically: true, encoding: .utf8)
        let kernel = try await startKernel(workdir: tmp.path)
        #expect(kernel.root?.id.value == "kernel_state")
        #expect(kernel.root?.handlerModule == "kernel_state.tools")
        #expect(
            kernel.root?.metaValue(forKey: "guid")?.asString
                == "0123456789abcdef0123456789abcdef")
    }

    /// The daemon's boot pass walks the REGISTRATION-ordered index,
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
        let verbs = kernel.rows.all().filter { $0["agent"]?.asString == "kernel_state" }
            .filter { $0["kind"]?.asString == "call" }
            .compactMap { $0["verb"]?.asString }
        #expect(verbs == ["boot"], "the boot pass minted \(verbs)")
        await kernel.shutdown()
    }
}

private func makeTempDir() -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(
        "fantastic-bootstrap-test-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}
