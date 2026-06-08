// cloud_bridge transport — in-process mTLS round-trip over a MemoryByteChannel.
//
// The Swift analog of Rust's `cloud_bridge_tls_loopback_round_trip`: two
// CloudBridgeTransport legs (client + server) handshake over an in-memory byte
// channel pair, then tunnel kernel-bridge frames. Exercises the hand-built
// Ed25519 cert (CloudCert), the NIOSSL TLS-1.3 mutual-auth handshake driven over
// buffers, length framing, symmetric inbound `call` dispatch, AND a >64 KiB
// frame (multi-TLS-record reassembly). Uses ONLY the public API (no `@testable`)
// and is fully async (no `.wait()` / DispatchSemaphore) so it composes cleanly
// with the concurrency runtime. `pinRejectsUnapprovedPeerCert` covers the
// negative path (a peer cert not in `approved_peer_certs` aborts the handshake).

import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import Testing

/// Minimal echo agent — returns the inbound payload wrapped, so a large blob can
/// be verified intact across the TLS + framing round-trip.
private final class EchoBundle: AgentBundle {
    let name = "echo"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["echoed": payload])
    }
}

private func makeKernel(withEcho: Bool) async -> Kernel {
    let registry = BundleRegistry()
    if withEcho { registry.register("echo.tools", EchoBundle()) }
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "core", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    if withEcho {
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "echo.tools", "id": "echo"])
    }
    return kernel
}

/// Standard PEM wrapper for a cert DER (so a peer can pin it).
private func derToPEM(_ der: [UInt8]) -> String {
    let b64 = Data(der).base64EncodedString()
    var s = "-----BEGIN CERTIFICATE-----\n"
    var i = b64.startIndex
    while i < b64.endIndex {
        let j = b64.index(i, offsetBy: 64, limitedBy: b64.endIndex) ?? b64.endIndex
        s += b64[i..<j] + "\n"
        i = j
    }
    s += "-----END CERTIFICATE-----\n"
    return s
}

private struct TestDeadlineExceeded: Error { let seconds: UInt64 }

/// Run `body` but fail fast if it doesn't finish within `seconds` (a deadlock
/// safety net — swift-testing's `.timeLimit` is 1-minute-granular).
private func withDeadline<T: Sendable>(
    _ seconds: UInt64, _ body: @escaping @Sendable () async throws -> T
) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask { try await body() }
        group.addTask {
            try await Task.sleep(nanoseconds: seconds * 1_000_000_000)
            throw TestDeadlineExceeded(seconds: seconds)
        }
        let r = try await group.next()!
        group.cancelAll()
        return r
    }
}

@Suite("cloud_bridge transport")
struct CloudBridgeTransportTests {
    @Test func mTLSRoundTripOverMemoryChannel() async throws {
        try await withDeadline(15) {
            let kernelA = await makeKernel(withEcho: false)
            let kernelB = await makeKernel(withEcho: true)

            let id1 = [UInt8](repeating: 7, count: 32)
            let id2 = [UInt8](repeating: 9, count: 32)
            let (cert1, key1) = try CloudCert.selfSigned(idKey: id1)
            let (cert2, key2) = try CloudCert.selfSigned(idKey: id2)
            let pem1 = derToPEM(cert1)
            let pem2 = derToPEM(cert2)

            let (chA, chB) = await MemoryByteChannel.pair()

            // Both legs handshake concurrently (each blocks awaiting the peer).
            async let ta = CloudBridgeTransport.connect(
                channel: chA, server: false, certDER: cert1, keyPKCS8: key1,
                approvedPeerPEMs: [pem2], localAgentId: "brA", localKernel: kernelA)
            async let tb = CloudBridgeTransport.connect(
                channel: chB, server: true, certDER: cert2, keyPKCS8: key2,
                approvedPeerPEMs: [pem1], localAgentId: "brB", localKernel: kernelB)
            let (transportA, transportB) = try await (ta, tb)

            // 1) forward A → B's "echo" agent → reply back to A.
            let r1 = await transportA.forward(
                target: "echo", payload: ["type": "echo", "msg": "hi"])
            #expect(r1["echoed"]["msg"].asString == "hi")

            // 2) >64 KiB frame — multi-TLS-record reassembly through the framing.
            let big = String(repeating: "x", count: 200_000)
            let r2 = await transportA.forward(
                target: "echo", payload: ["type": "echo", "blob": .string(big)])
            #expect(r2["echoed"]["blob"].asString?.count == big.count)

            // 3) reverse direction: B → A's "core" (both legs serve inbound calls).
            let r3 = await transportB.forward(target: "core", payload: ["type": "list_agents"])
            let names = (r3["agents"].asArray ?? []).compactMap { $0["id"].asString }
            #expect(names.contains("core"))

            await transportA.close()
            await transportB.close()
        }
    }

    @Test func pinRejectsUnapprovedPeerCert() async throws {
        try await withDeadline(15) {
            let id1 = [UInt8](repeating: 11, count: 32)
            let id2 = [UInt8](repeating: 22, count: 32)
            let id3 = [UInt8](repeating: 33, count: 32)
            let (cert1, key1) = try CloudCert.selfSigned(idKey: id1)
            let (cert2, key2) = try CloudCert.selfSigned(idKey: id2)
            let (cert3, _) = try CloudCert.selfSigned(idKey: id3)
            let pem1 = derToPEM(cert1)
            let pem3 = derToPEM(cert3)

            let (chA, chB) = await MemoryByteChannel.pair()

            // Server B correctly approves A's cert. Detached + best-effort: it
            // sends its cert (which A rejects), then is released when we close
            // the channels — so it never hangs the test.
            let bTask = Task {
                try? await CloudBridgeTransport.connect(
                    channel: chB, server: true, certDER: cert2, keyPKCS8: key2,
                    approvedPeerPEMs: [pem1])
            }

            // Client A approves cert3 — NOT B's cert2 — so it MUST reject the
            // peer cert mid-handshake (NIOSSL chain verification against the
            // pinned trust roots fails → writeInbound throws → connect throws).
            var rejected = false
            do {
                _ = try await CloudBridgeTransport.connect(
                    channel: chA, server: false, certDER: cert1, keyPKCS8: key1,
                    approvedPeerPEMs: [pem3])
            } catch {
                rejected = true
            }
            #expect(rejected, "client must reject a peer cert not in approved_peer_certs")

            await chA.close()
            await chB.close()
            _ = await bTask.value
        }
    }
}
