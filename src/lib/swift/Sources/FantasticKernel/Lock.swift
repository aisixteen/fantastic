// Workdir lock — python `kernel/_lock.py`.
//
// The decision is an `flock(LOCK_EX)` on `<workdir>/.fantastic/lock.yaml`,
// held on an open descriptor for the life of the process: the OS grants it to
// exactly one opener and drops it the moment its holder exits — however it
// exits — so a dead holder's lock is simply taken over. Python, rust and swift
// take the SAME flock on the SAME file, so kernels of different runtimes
// exclude each other too.
//
// The file holds `pid: <pid>` — python's own `yaml.dump({"pid": pid})` bytes
// (`YamlIO.dump`) — for the refusal text, never for the decision. It is in the
// file BEFORE the file is `lock.yaml`: the pid goes into a private
// `lock.yaml.tmp.<hex>`, which is flocked and then hard-linked to `lock.yaml`
// (atomic, refused when the name exists), so no opener ever reads a lock
// without its pid. A `lock.yaml` nobody holds (its holder died) is replaced by
// renaming the staged file over it while holding the dead file's flock — a
// second taker of the same dead file finds it held. `release()` removes the
// file only while it still names OUR pid (python's `release_lock`).
//
// The one-shot CLI paths (`call`, `init`) take this same flock and refuse on
// `.alreadyRunning` — they never judge a pid alive.

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
    private let workdir: URL
    /// The descriptor whose flock IS the lock; open while it is held.
    private var fileDescriptor: Int32 = -1

    public init(workdir: URL) {
        self.workdir = workdir
        self.lockFile =
            workdir
            .appendingPathComponent(".fantastic")
            .appendingPathComponent("lock.yaml")
    }

    /// Take the flock on `.fantastic/lock.yaml` for the life of this process.
    /// Throws `.alreadyRunning` naming the pid in the file if a live process
    /// holds it; a lock whose holder died is taken over.
    public func acquire() throws {
        // A `.fantastic` that IS the fantastic home is never a kernel's: refused
        // before anything is written there.
        try refuseHomeAsStateDir(
            workdir: workdir.path, stateDir: lockFile.deletingLastPathComponent().path,
            home: fantasticHome())
        // Ensure parent dir exists.
        try? FileManager.default.createDirectory(
            at: lockFile.deletingLastPathComponent(),
            withIntermediateDirectories: true)

        let staged =
            lockFile.path + ".tmp." + String(format: "%08x", UInt32.random(in: .min ... .max))
        let fd = open(staged, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0o644)
        guard fd >= 0 else {
            throw LockError.openFailed("open \(staged): errno=\(errno)")
        }
        do {
            // python's own `yaml.dump({"pid": N})` bytes (`pid: N\n`).
            let body = Array(YamlIO.dump(.object(["pid": .integer(Int64(getpid()))])).utf8)
            let written = write(fd, body, body.count)
            if written != body.count {
                throw LockError.writeFailed("partial write: \(written)/\(body.count)")
            }
            // uncontended: the staged file is ours alone
            if flock(fd, LOCK_EX) != 0 { throw LockError.flockFailed(errno: errno) }
            while try !publish(staged) {}
        } catch {
            close(fd)
            unlink(staged)
            throw error
        }
        // After a link the staged name is a second name for `lock.yaml`; after
        // a rename it is already gone.
        unlink(staged)
        self.fileDescriptor = fd
    }

    /// One attempt to make the staged (flocked, pid-filled) file `lock.yaml` —
    /// python `_publish`. `true` once it is; `false` when `lock.yaml` changed
    /// under us (try again); throws `.alreadyRunning` when a live process holds it.
    private func publish(_ staged: String) throws -> Bool {
        let path = lockFile.path
        if link(staged, path) == 0 { return true }
        guard errno == EEXIST else { throw LockError.openFailed("link \(path): errno=\(errno)") }
        let cur = open(path, O_RDONLY | O_CLOEXEC)
        if cur < 0 {
            // its holder released it between the two calls
            if errno == ENOENT { return false }
            throw LockError.openFailed("open \(path): errno=\(errno)")
        }
        defer { close(cur) }
        if flock(cur, LOCK_EX | LOCK_NB) != 0 {
            let flockErrno = errno
            guard flockErrno == EWOULDBLOCK else { throw LockError.flockFailed(errno: flockErrno) }
            // Held: a live owner — or a taker still replacing a dead lock, whose
            // file (and dead pid) this racer then reads. Either way, refused.
            guard let pid = Self.parsePid(Self.readAll(cur)) else {
                // python's `lock file at .fantastic/lock.yaml: lock.yaml malformed`.
                throw LockError.openFailed("lock file at .fantastic/lock.yaml: lock.yaml malformed")
            }
            throw LockError.alreadyRunning(
                pid: pid,
                workdir: lockFile.deletingLastPathComponent().deletingLastPathComponent().path)
        }
        // Nobody holds it: its holder died. Replace it — but only while it is
        // still the file `lock.yaml` names (another taker may have replaced it).
        var held = stat()
        var named = stat()
        guard fstat(cur, &held) == 0 else {
            throw LockError.openFailed("fstat \(path): errno=\(errno)")
        }
        if stat(path, &named) != 0 {
            if errno == ENOENT { return false }
            throw LockError.openFailed("stat \(path): errno=\(errno)")
        }
        guard held.st_dev == named.st_dev, held.st_ino == named.st_ino else { return false }
        guard rename(staged, path) == 0 else {
            throw LockError.writeFailed("rename \(staged): errno=\(errno)")
        }
        return true
    }

    /// Release the lock. Safe to call multiple times; a no-op when this
    /// instance holds none. Removes the FILE only while it still names OUR pid
    /// (python's `release_lock`) — a file replaced by hand is not ours to
    /// delete — then closes the descriptor, which drops the flock.
    public func release() {
        guard fileDescriptor >= 0 else { return }
        if Self.currentHolder(lockFile) == getpid() {
            try? FileManager.default.removeItem(at: lockFile)
        }
        close(fileDescriptor)
        fileDescriptor = -1
    }

    deinit { release() }

    // MARK: - Helpers

    /// The pid `<workdir>/.fantastic/lock.yaml` records, or `nil` for a
    /// missing/unparseable file — python's `_read_lock`.
    private static func currentHolder(_ lockFile: URL) -> pid_t? {
        (try? Data(contentsOf: lockFile)).flatMap { parsePid(Array($0)) }
    }

    /// A lock file's recorded pid, or `nil` when the bytes are not a YAML
    /// mapping with an integer `pid`. NOT bounds-checked against `pid_t` with
    /// a trapping cast: a malformed huge value reads as no valid pid rather
    /// than crashing the acquiring process.
    private static func parsePid(_ bytes: [UInt8]) -> pid_t? {
        guard let text = String(bytes: bytes, encoding: .utf8),
            case .object(let doc)? = try? YamlIO.load(text),
            let pid = doc["pid"]?.asInt
        else {
            return nil
        }
        return pid_t(exactly: pid)
    }

    /// Everything readable from `fd`.
    private static func readAll(_ fd: Int32) -> [UInt8] {
        var out: [UInt8] = []
        var chunk = [UInt8](repeating: 0, count: 4096)
        while true {
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 { return out }
            out.append(contentsOf: chunk[0..<n])
        }
    }

}
