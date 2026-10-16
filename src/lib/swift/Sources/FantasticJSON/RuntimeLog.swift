// The runtime's stderr line — python's `logging.lastResort`.
//
// Python configures NO logging handler anywhere in the kernel or the bundles
// (`basicConfig` / `addHandler` / `dictConfig` appear nowhere), so a
// `logger.warning(...)` / `logger.error(...)` falls through to
// `logging.lastResort`, which writes the BARE formatted message — no timestamp,
// no level, no logger name — to stderr. `logger.info` prints nothing at all.
// The e2e kernel-log dumps and any spawned CLI coder read exactly those lines,
// so a port that renders them through a structured logger (or drops them) is a
// visible divergence, not a cosmetic one.
//
// Lives beside `PyFormat.swift` — this module is where the shared
// python-behaviour helpers live, and every bundle already depends on it.
//
// `sink` is the test seam the twins use: with one installed the line goes there
// instead of fd 2, so a suite can assert the text a failure printed.

import Foundation

public enum RuntimeLog {
    private static let lock = NSLock()
    /// Every ACTIVE capture, not one: two suites can be capturing at the same
    /// time (tests run in parallel), and a single slot let the second one's
    /// teardown silently unhook the first.
    nonisolated(unsafe) private static var sinks: [LineBox] = []

    /// One `logger.warning` line: the message, a newline, stderr.
    public static func warn(_ line: String) {
        lock.lock()
        let active = sinks
        lock.unlock()
        if active.isEmpty {
            FileHandle.standardError.write((line + "\n").data(using: .utf8) ?? Data())
            return
        }
        for box in active { box.append(line + "\n") }
    }

    /// Collect every `warn` line emitted while `body` runs. Test-only seam.
    ///
    /// `NSLock` is unavailable from an async context, so attaching and detaching
    /// the box happens in synchronous helpers.
    public static func capture(_ body: () async throws -> Void) async rethrows -> [String] {
        let box = LineBox()
        attach(box)
        defer { detach(box) }
        try await body()
        return box.lines()
    }

    /// Every ACTIVE stdout capture — the operator-facing channel, separate from
    /// the runtime's own WARN channel above.
    nonisolated(unsafe) private static var outSinks: [LineBox] = []

    /// One stdout line, FLUSHED — python's `print(..., flush=True)`. stdout is
    /// block-buffered on a pipe, and an operator-facing line the process is
    /// signalled right after must not be left in the buffer.
    public static func out(_ line: String) {
        lock.lock()
        let active = outSinks
        lock.unlock()
        if active.isEmpty {
            print(line)
            fflush(nil)  // every output stream, stdout included (Glibc's `stdout` is a mutable global)
            return
        }
        for box in active { box.append(line) }
    }

    /// Collect every `out` line emitted while `body` runs. Test-only seam.
    public static func captureOut(_ body: () async throws -> Void) async rethrows -> [String] {
        let box = LineBox()
        attachOut(box)
        defer { detachOut(box) }
        try await body()
        return box.lines()
    }

    private static func attachOut(_ box: LineBox) {
        lock.lock()
        outSinks.append(box)
        lock.unlock()
    }

    private static func detachOut(_ box: LineBox) {
        lock.lock()
        outSinks.removeAll { $0 === box }
        lock.unlock()
    }

    private static func attach(_ box: LineBox) {
        lock.lock()
        sinks.append(box)
        lock.unlock()
    }

    private static func detach(_ box: LineBox) {
        lock.lock()
        sinks.removeAll { $0 === box }
        lock.unlock()
    }

    private final class LineBox: @unchecked Sendable {
        private let lock = NSLock()
        private var buf: [String] = []
        func append(_ s: String) {
            lock.lock()
            buf.append(s)
            lock.unlock()
        }
        func lines() -> [String] {
            lock.lock()
            defer { lock.unlock() }
            return buf
        }
    }
}
