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

/// Bind :0 on loopback, read back what the OS chose, release it.
func freeTCPPort() -> UInt16 {
    let fd = socket(AF_INET, Int32(SOCK_STREAM), 0)
    guard fd >= 0 else { return 0 }
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
    guard bound == 0 else { return 0 }
    var out = sockaddr_in()
    var len = socklen_t(MemoryLayout<sockaddr_in>.size)
    let named = withUnsafeMutablePointer(to: &out) { p in
        p.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(fd, $0, &len) }
    }
    guard named == 0 else { return 0 }
    return UInt16(bigEndian: out.sin_port)
}

/// An in-memory kernel whose `web` record names a free port.
func startWebKernel() async throws -> Kernel {
    try await startKernelInMemory(portHint: freeTCPPort())
}
