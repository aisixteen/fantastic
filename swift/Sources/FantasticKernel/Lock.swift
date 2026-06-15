// POSIX flock-based workdir lock.
//
// Mirrors Rust's `fantastic-kernel/src/lock.rs`. Writes
// `<workdir>/.fantastic/lock.json` with `{"pid": <pid>}` and holds an
// exclusive flock on the file for the lifetime of the kernel process.
// On boot we check whether an existing lock's `pid` is still alive
// via `kill(pid, 0)` — stale locks (dead pid) get overwritten.

#if canImport(Darwin)
    import Darwin
#else
    import Glibc
#endif
import FantasticJSON
import Foundation

public final class WorkdirLock: @unchecked Sendable {
    public enum LockError: Error, Sendable {
        case alreadyRunning(pid: pid_t, workdir: String)
        case openFailed(String)
        case flockFailed(errno: Int32)
        case writeFailed(String)
    }

    private let lockFile: URL
    private var fileDescriptor: Int32 = -1

    public init(workdir: URL) {
        self.lockFile = workdir
            .appendingPathComponent(".fantastic")
            .appendingPathComponent("lock.json")
    }

    /// Acquire the lock. Throws `.alreadyRunning` if a live process
    /// already holds it. Stale locks (dead pid) are silently
    /// reclaimed.
    public func acquire() throws {
        // Ensure parent dir exists.
        try? FileManager.default.createDirectory(
            at: lockFile.deletingLastPathComponent(),
            withIntermediateDirectories: true)

        // Open or create the lock file.
        let fd = open(lockFile.path, O_RDWR | O_CREAT, 0o644)
        guard fd >= 0 else {
            throw LockError.openFailed("open \(lockFile.path): errno=\(errno)")
        }

        // Try to acquire an exclusive non-blocking lock.
        let rc = flock(fd, LOCK_EX | LOCK_NB)
        if rc != 0 {
            // Failed — check if existing holder is alive via the pid
            // recorded in the file.
            close(fd)
            if let existingPid = readExistingPid(), existingPid > 0 {
                if processAlive(existingPid) {
                    throw LockError.alreadyRunning(
                        pid: existingPid,
                        workdir: lockFile.deletingLastPathComponent()
                            .deletingLastPathComponent().path
                    )
                }
                // Stale — try once more after truncating the file.
                let fd2 = open(lockFile.path, O_RDWR | O_CREAT | O_TRUNC, 0o644)
                guard fd2 >= 0 else {
                    throw LockError.openFailed("reopen after stale: errno=\(errno)")
                }
                if flock(fd2, LOCK_EX | LOCK_NB) != 0 {
                    close(fd2)
                    throw LockError.flockFailed(errno: errno)
                }
                self.fileDescriptor = fd2
            } else {
                throw LockError.flockFailed(errno: errno)
            }
        } else {
            self.fileDescriptor = fd
        }

        // Write our pid into the file.
        let pid = getpid()
        let json: JSON = .object(["pid": .integer(Int64(pid))])
        let body = json.serialize()
        // Truncate then write so old contents (from stale-takeover path)
        // don't trail off the end.
        ftruncate(fileDescriptor, 0)
        lseek(fileDescriptor, 0, SEEK_SET)
        guard let data = body.data(using: .utf8) else {
            throw LockError.writeFailed("serialize")
        }
        let written = data.withUnsafeBytes { ptr in
            write(fileDescriptor, ptr.baseAddress, data.count)
        }
        if written != data.count {
            throw LockError.writeFailed("partial write: \(written)/\(data.count)")
        }
    }

    /// Release the lock. Safe to call multiple times.
    public func release() {
        guard fileDescriptor >= 0 else { return }
        flock(fileDescriptor, LOCK_UN)
        close(fileDescriptor)
        fileDescriptor = -1
        // Best-effort cleanup of the lock file. If another process
        // is already racing to take it, that's fine — they'll
        // truncate + write their own pid.
        try? FileManager.default.removeItem(at: lockFile)
    }

    deinit { release() }

    // MARK: - Helpers

    private func readExistingPid() -> pid_t? {
        guard let data = try? Data(contentsOf: lockFile),
            let json = try? JSON.parse(data),
            let pid = json["pid"].asInt
        else {
            return nil
        }
        return pid_t(pid)
    }

    /// `kill(pid, 0)` — checks signal delivery without sending one.
    /// Returns true if the process exists and we can signal it.
    private func processAlive(_ pid: pid_t) -> Bool {
        let result = kill(pid, 0)
        if result == 0 { return true }
        // ESRCH means "no such process"; any other errno (e.g.
        // EPERM) means the process exists but is owned by someone
        // else — still counts as alive.
        return errno != ESRCH
    }
}
