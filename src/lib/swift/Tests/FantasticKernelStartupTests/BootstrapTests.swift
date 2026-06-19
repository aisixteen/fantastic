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
        let listed = await kernel.send(
            AgentId("core"),
            .object(["type": .string("list_agents")]))
        let ids = (listed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("core"))
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
        let listed = await probe.send(
            AgentId("core"),
            .object(["type": .string("list_agents")]))
        let ids = (listed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("core"))
    }
}

private func makeTempDir() -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(
        "fantastic-bootstrap-test-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}
