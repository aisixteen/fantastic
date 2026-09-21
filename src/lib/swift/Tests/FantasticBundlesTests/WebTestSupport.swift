// Shared helpers for the web/HTTP/WS suites.
//
// `web` binds EXACTLY `rec.port` (python's `_spawn` requires it, with no
// default and no "any free port" hint — A11.24), so a test that wants a live
// listener has to name a port. Ask the OS for a free one first.

import FantasticKernel
import FantasticKernelStartup
import Foundation

#if canImport(Darwin)
    import Darwin
#else
    import Glibc
#endif

/// Ports this process has already handed to a test. The OS re-offers an
/// ephemeral port the moment the probe socket closes, so two tests running
/// concurrently could be handed the SAME number and then race to bind it. A
/// handed-out port is never handed out twice.
private final class PortLedger: @unchecked Sendable {
    private let lock = NSLock()
    private var taken: Set<UInt16> = []
    /// `true` when this port was free to claim.
    func claim(_ p: UInt16) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return taken.insert(p).inserted
    }
}

private let portLedger = PortLedger()

/// One probe: bind :0 on loopback, read back what the OS chose, release it.
/// Returns 0 only when a syscall failed, which the caller reports.
private func probeTCPPort() -> (port: UInt16, err: Int32) {
    let fd = socket(AF_INET, Int32(SOCK_STREAM), 0)
    guard fd >= 0 else { return (0, errno) }
    defer { close(fd) }
    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_port = 0
    addr.sin_addr.s_addr = inet_addr("127.0.0.1")
    let bound = withUnsafePointer(to: &addr) { p in
        p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }
    guard bound == 0 else { return (0, errno) }
    var out = sockaddr_in()
    var len = socklen_t(MemoryLayout<sockaddr_in>.size)
    let named = withUnsafeMutablePointer(to: &out) { p in
        p.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(fd, $0, &len) }
    }
    guard named == 0 else { return (0, errno) }
    return (UInt16(bigEndian: out.sin_port), 0)
}

/// A loopback port no other test in this process has been given.
///
/// FAILS LOUD. This used to answer 0 on any syscall failure, and 0 is a legal
/// `portHint`: the kernel then reported `httpPort() == 0` and a test built
/// `http://127.0.0.1:0/...` — the measured flake. There is no sensible port to
/// fall back to, so an exhausted probe stops the run instead.
func freeTCPPort() -> UInt16 {
    var lastErr: Int32 = 0
    for _ in 0..<64 {
        let (p, err) = probeTCPPort()
        if p == 0 {
            lastErr = err
            continue
        }
        if portLedger.claim(p) { return p }
    }
    fatalError(
        "freeTCPPort: no unclaimed loopback port after 64 probes (last errno \(lastErr))")
}

/// An in-memory kernel whose `web` record names a free port.
func startWebKernel() async throws -> Kernel {
    try await startKernelInMemory(portHint: freeTCPPort())
}
