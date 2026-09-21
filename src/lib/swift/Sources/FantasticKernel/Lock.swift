// PID-ONLY workdir lock — python `kernel/_lock.py`.
//
// C82: the ONE decision is the recorded pid's liveness, exactly as python's
// `_pid_alive` makes it (`kill(pid,0)`: alive on success OR `EPERM` — the
// process exists and is owned by someone else — dead on `ESRCH` or any other
// error; `pid <= 0` is always dead). Python holds NO flock at all — a bare
// pid file, checked and rewritten by plain read/write. The `flock` this class
// still takes is an ADDITIONAL guard against two SWIFT processes racing the
// same open, never the admission decision: checking flock success FIRST (the
// previous shape here) let an uncontested flock silently overwrite a live
// python or rust daemon's lock, since neither of those takes one. So the pid
// check always runs BEFORE any flock attempt, and a flock failure can only
// make acquisition MORE conservative, never less.
//
// Writes `<workdir>/.fantastic/lock.json` with `{"pid": <pid>}` — python's
// own `json.dumps` bytes (`pyJsonDumps`, WITH the space after `:`; C50). And
// `release()` removes the file only when it still names OUR pid (python's
// `release_lock`) — a second kernel that reclaimed a stale lock now owns
// the file, and this process's release must not delete THAT lock.

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
        self.lockFile =
            workdir
            .appendingPathComponent(".fantastic")
            .appendingPathComponent("lock.json")
    }

    /// Acquire the lock. Throws `.alreadyRunning` if a live process
    /// already holds it (checked from the FILE's recorded pid alone, C82).
    /// Stale locks (dead pid, or no file at all) are silently reclaimed.
    public func acquire() throws {
        // Ensure parent dir exists.
        try? FileManager.default.createDirectory(
            at: lockFile.deletingLastPathComponent(),
            withIntermediateDirectories: true)

        // THE ONE DECISION, made first and unconditionally: a live recorded
        // pid refuses, whether or not anything holds this file's flock (a
        // python or rust daemon holds none at all).
        if let existingPid = readExistingPid(), pidAlive(existingPid) {
            throw LockError.alreadyRunning(
                pid: existingPid,
                workdir: lockFile.deletingLastPathComponent()
                    .deletingLastPathComponent().path
            )
        }

        // Safe to take: open (create if absent) and flock as a same-runtime
        // race guard ONLY — its failure here can only refuse MORE often
        // than python would, never admit a live pid python would refuse.
        let fd = open(lockFile.path, O_RDWR | O_CREAT, 0o644)
        guard fd >= 0 else {
            throw LockError.openFailed("open \(lockFile.path): errno=\(errno)")
        }
        if flock(fd, LOCK_EX | LOCK_NB) != 0 {
            let flockErrno = errno
            close(fd)
            throw LockError.flockFailed(errno: flockErrno)
        }
        self.fileDescriptor = fd

        // Write our pid into the file — python's own `json.dumps({"pid": N})`
        // bytes (C50: WITH the space after `:`, `pyJsonDumps`'s compact
        // separators, not this runtime's serde-shaped `serialize()`).
        let pid = getpid()
        let body = pyJsonDumps(.object(["pid": .integer(Int64(pid))]))
        // Truncate then write so old contents (from a stale-lock takeover)
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

    /// Release the lock. Safe to call multiple times. Removes the FILE only
    /// when it still names OUR pid (python's `release_lock`) — a second
    /// kernel that reclaimed a stale lock in the meantime now owns it, and
    /// this call must not delete that live lock out from under it.
    public func release() {
        let owned = readExistingPid() == getpid()
        if fileDescriptor >= 0 {
            flock(fileDescriptor, LOCK_UN)
            close(fileDescriptor)
            fileDescriptor = -1
        }
        if owned {
            try? FileManager.default.removeItem(at: lockFile)
        }
    }

    deinit { release() }

    // MARK: - Helpers

    /// The file's recorded pid, or `nil` for a missing/unparseable file —
    /// python's `_read_lock`. NOT bounds-checked against `pid_t` with a
    /// trapping cast: a malformed huge value reads as absent-of-a-valid-pid
    /// (dead) rather than crashing the acquiring process.
    private func readExistingPid() -> pid_t? {
        guard let data = try? Data(contentsOf: lockFile),
            let json = try? JSON.parse(data),
            let pid = json["pid"].asInt
        else {
            return nil
        }
        return pid_t(exactly: pid)
    }

    /// Python's `_pid_alive`: `pid <= 0` is always dead; `kill(pid, 0)`
    /// success or `EPERM` (the process exists, owned by someone else) is
    /// alive; `ESRCH` (no such process) or any other errno is dead.
    private func pidAlive(_ pid: pid_t) -> Bool {
        if pid <= 0 { return false }
        if kill(pid, 0) == 0 { return true }
        return errno == EPERM
    }
}
