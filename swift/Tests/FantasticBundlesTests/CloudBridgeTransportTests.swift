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

import FantasticFile
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

    @Test func binaryForwardStreamsRawBytesOverCloud() async throws {
        try await withDeadline(20) {
            // The CLOUD wire half of cross-kernel streaming — a tag-1 binary
            // record `[4B len|tag|wire]` tunnelled over the mTLS relay to a
            // remote file_bridge. Raw bytes both ways, never base64. (WS variant:
            // `wsBinaryForwardStreamsRawBytesOverWire`.)
            let kernelA = await makeKernel(withEcho: false)
            // kernelB hosts an OPEN file_bridge in a cwd-relative dir.
            let registry = BundleRegistry()
            registry.register("file_bridge.tools", FileBundle())
            let kernelB = Kernel(storage: .inMemory, bundles: registry)
            let rootB = Agent(id: "core", handlerModule: nil, parentId: nil)
            kernelB.register(rootB)
            kernelB.setRoot(rootB)
            let fm = FileManager.default
            let rel = "fantastic-cloudbrstream-\(UUID().uuidString)"
            let dir = URL(fileURLWithPath: fm.currentDirectoryPath)
                .appendingPathComponent(rel)
            try fm.createDirectory(at: dir, withIntermediateDirectories: true)
            defer { try? fm.removeItem(at: dir) }
            _ = await kernelB.send(
                "core",
                [
                    "type": "create_agent", "handler_module": "file_bridge.tools",
                    "id": "fs", "root": .string(rel), "ingress_rule": "allow_all",
                ])

            let id1 = [UInt8](repeating: 7, count: 32)
            let id2 = [UInt8](repeating: 9, count: 32)
            let (cert1, key1) = try CloudCert.selfSigned(idKey: id1)
            let (cert2, key2) = try CloudCert.selfSigned(idKey: id2)
            let pem1 = derToPEM(cert1)
            let pem2 = derToPEM(cert2)
            let (chA, chB) = await MemoryByteChannel.pair()
            async let ta = CloudBridgeTransport.connect(
                channel: chA, server: false, certDER: cert1, keyPKCS8: key1,
                approvedPeerPEMs: [pem2], localAgentId: "brA", localKernel: kernelA)
            async let tb = CloudBridgeTransport.connect(
                channel: chB, server: true, certDER: cert2, keyPKCS8: key2,
                approvedPeerPEMs: [pem1], localAgentId: "brB", localKernel: kernelB)
            let (transportA, transportB) = try await (ta, tb)

            let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
            // write_stream (binary) over the relay tunnel → kernelB.fs
            let (w, _) = await transportA.binaryForward(
                target: "fs",
                header: [
                    "type": "write_stream", "path": "blob.bin", "truncate": .bool(true),
                ],
                blob: payload)
            #expect(w["written"].asInt == Int64(payload.count), "\(w)")
            // read_stream (binary) over the relay tunnel → raw bytes back
            let (_, body) = await transportA.binaryForward(
                target: "fs",
                header: ["type": "read_stream", "path": "blob.bin"],
                blob: Data())
            #expect(body == payload, "raw bytes must round-trip over the cloud relay tunnel")

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

    // ── authorization seam (ingress/egress rules) ───────────────────

    @Test func ingressRulesDecideCorrectly() {
        let call = AuthAction(kind: "call", target: "t", verb: "reflect")
        let watch = AuthAction(kind: "watch", target: "t", verb: "watch")
        // AllowAll — true no-op.
        if case .deny = IngressRules.AllowAll().authorize(call) {
            Issue.record("allow_all denied a call")
        }
        // DenyInbound — refuses `call`, permits watch/unwatch (already ignored).
        guard case .deny = IngressRules.DenyInbound().authorize(call) else {
            Issue.record("deny_inbound permitted a call")
            return
        }
        if case .deny = IngressRules.DenyInbound().authorize(watch) {
            Issue.record("deny_inbound gated a watch (should be denied-by-omission)")
        }
    }

    @Test func ingressRegistryResolvesByName() throws {
        // SEALED BY DEFAULT — absent / null / empty ⇒ DenyInbound.
        let call = AuthAction(kind: "call", target: "t", verb: "v")
        for value in [JSON?.none, .some(.null), .some(.string(""))] {
            guard case .deny = try IngressRules.resolve(value).authorize(call) else {
                Issue.record("absent/null/empty must SEAL (deny) by default")
                return
            }
        }
        // explicit allow_all ⇒ admit.
        if case .deny = try IngressRules.resolve("allow_all").authorize(call) {
            Issue.record("allow_all must admit")
        }
        // string + object form (both `type` and legacy `policy`) resolve DenyInbound
        for value: JSON in ["deny_inbound", ["type": "deny_inbound"], ["policy": "deny_inbound"]] {
            guard case .deny = try IngressRules.resolve(value).authorize(call) else {
                Issue.record("deny_inbound (\(value)) did not deny a call")
                return
            }
        }
        // egress: inbound-only names ⇒ Silent (present nothing); unknown ⇒ throws
        #expect(try EgressRules.resolve("deny_inbound").credential() == nil)
        #expect(throws: AuthPolicyError.self) { try IngressRules.resolve("nope") }
        #expect(throws: AuthPolicyError.self) { try EgressRules.resolve("nope") }
    }

    @Test func denyInboundRefusesReverseCall() async throws {
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

            // A's leg serves `deny_inbound`; B's leg is the default allow_all. So
            // A→B forwards succeed, but B→A reverse calls are refused on arrival.
            async let ta = CloudBridgeTransport.connect(
                channel: chA, server: false, certDER: cert1, keyPKCS8: key1,
                approvedPeerPEMs: [pem2], localAgentId: "brA", localKernel: kernelA,
                ingress: IngressRules.DenyInbound())
            async let tb = CloudBridgeTransport.connect(
                channel: chB, server: true, certDER: cert2, keyPKCS8: key2,
                approvedPeerPEMs: [pem1], localAgentId: "brB", localKernel: kernelB)
            let (transportA, transportB) = try await (ta, tb)

            // Forward direction (A → B's allow_all leg) still works — the no-op guard.
            let fwd = await transportA.forward(
                target: "echo", payload: ["type": "echo", "msg": "hi"])
            #expect(fwd["echoed"]["msg"].asString == "hi")

            // Reverse direction (B → A's deny_inbound leg) is refused on arrival.
            let rev = await transportB.forward(target: "core", payload: ["type": "list_agents"])
            #expect(
                rev["reason"].asString == "unauthorized",
                "reverse call should be denied: \(rev)")

            await transportA.close()
            await transportB.close()
        }
    }
}

/// The `password` policy mutates a process-global env var, so this suite is
/// `.serialized` (its tests don't run concurrently) and each uses a UNIQUE env var.
@Suite("cloud_bridge password auth", .serialized)
struct CloudBridgePasswordTests {
    private func mkAction(_ token: String?) -> AuthAction {
        AuthAction(kind: "call", target: "t", verb: "reflect", token: token)
    }

    @Test func passwordChecksTokenAndPresentsCredential() throws {
        let env = "FANTASTIC_GROUP_TOKEN_SWIFT_UNIT"
        setenv(env, "s3cret", 1)
        defer { unsetenv(env) }
        // ingress side CHECKS the envelope token (new `env` spelling threads through)
        let ing = try IngressRules.resolve(["type": "password", "env": .string(env)])
        if case .deny = ing.authorize(mkAction("s3cret")) {
            Issue.record("matching token should allow")
        }
        guard case .deny = ing.authorize(mkAction("nope")) else {
            Issue.record("wrong token should deny")
            return
        }
        guard case .deny = ing.authorize(mkAction(nil)) else {
            Issue.record("missing token should deny")
            return
        }
        // egress side PRESENTS the same token (symmetric group)
        let eg = try EgressRules.resolve(["type": "password", "env": .string(env)])
        #expect(eg.credential() == "s3cret")
        // fail-closed / present-nothing when the env var is unset
        unsetenv(env)
        guard case .deny = ing.authorize(mkAction("s3cret")) else {
            Issue.record("unset env must fail closed")
            return
        }
        #expect(eg.credential() == nil)
    }

    @Test func registryResolvesPasswordAndName() throws {
        // bare string ⇒ default env var; object form threads token_env (legacy spelling)
        #expect(try IngressRules.resolve("password") is IngressRules.Password)
        let ing = try IngressRules.resolve(["type": "password", "token_env": "X"])
        #expect((ing as? IngressRules.Password)?.tokenEnv == "X")
        let eg = try EgressRules.resolve(["type": "password", "env": "X"])
        #expect((eg as? EgressRules.Password)?.tokenEnv == "X")
        // reflect surfaces only the rule NAME, never the config
        #expect(ruleName(["type": "password", "env": "X"], default: "allow_all") == "password")
        #expect(ruleName("deny_inbound", default: "allow_all") == "deny_inbound")
        #expect(ruleName(nil, default: "silent") == "silent")
    }

    @Test func passwordGroupMemberRoundTripsOverCloud() async throws {
        let env = "FANTASTIC_GROUP_TOKEN_SWIFT_OK"
        setenv(env, "s3cret", 1)
        defer { unsetenv(env) }
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
            // Both legs are group members with the SAME group token (symmetric:
            // ingress checks + egress presents).
            async let ta = CloudBridgeTransport.connect(
                channel: chA, server: false, certDER: cert1, keyPKCS8: key1,
                approvedPeerPEMs: [pem2], localAgentId: "brA", localKernel: kernelA,
                ingress: IngressRules.Password(tokenEnv: env),
                egress: EgressRules.Password(tokenEnv: env))
            async let tb = CloudBridgeTransport.connect(
                channel: chB, server: true, certDER: cert2, keyPKCS8: key2,
                approvedPeerPEMs: [pem1], localAgentId: "brB", localKernel: kernelB,
                ingress: IngressRules.Password(tokenEnv: env),
                egress: EgressRules.Password(tokenEnv: env))
            let (transportA, transportB) = try await (ta, tb)
            // A→B carries A's group token on the envelope; B accepts → echo works.
            let fwd = await transportA.forward(
                target: "echo", payload: ["type": "echo", "msg": "hi"])
            #expect(fwd["echoed"]["msg"].asString == "hi")
            // reverse works too (A is also a group member, accepts B's token)
            let rev = await transportB.forward(target: "core", payload: ["type": "list_agents"])
            let names = (rev["agents"].asArray ?? []).compactMap { $0["id"].asString }
            #expect(names.contains("core"), "reverse group call should dispatch: \(rev)")
            await transportA.close()
            await transportB.close()
        }
    }

    @Test func passwordRejectsTokenlessCallerOverCloud() async throws {
        let env = "FANTASTIC_GROUP_TOKEN_SWIFT_REJECT"
        setenv(env, "s3cret", 1)
        defer { unsetenv(env) }
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
            // A is an OUTSIDER (default ⇒ silent egress, presents no token); B requires it.
            async let ta = CloudBridgeTransport.connect(
                channel: chA, server: false, certDER: cert1, keyPKCS8: key1,
                approvedPeerPEMs: [pem2], localAgentId: "brA", localKernel: kernelA)
            async let tb = CloudBridgeTransport.connect(
                channel: chB, server: true, certDER: cert2, keyPKCS8: key2,
                approvedPeerPEMs: [pem1], localAgentId: "brB", localKernel: kernelB,
                ingress: IngressRules.Password(tokenEnv: env))
            let (transportA, transportB) = try await (ta, tb)
            // A→B presents no token → B's password gate refuses on arrival.
            let r = await transportA.forward(target: "echo", payload: ["type": "echo"])
            #expect(r["reason"].asString == "unauthorized", "tokenless caller must be denied: \(r)")
            await transportA.close()
            await transportB.close()
        }
    }

    @Test func asymmetricIngressEgressOverCloud() async throws {
        let env = "FANTASTIC_GROUP_TOKEN_SWIFT_ASYM"
        setenv(env, "fleet", 1)
        defer { unsetenv(env) }
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
            // A is a hub: refuse INBOUND, still PRESENT the fleet token outbound.
            // B is a group member that accepts the fleet token.
            async let ta = CloudBridgeTransport.connect(
                channel: chA, server: false, certDER: cert1, keyPKCS8: key1,
                approvedPeerPEMs: [pem2], localAgentId: "brA", localKernel: kernelA,
                ingress: IngressRules.DenyInbound(),
                egress: EgressRules.Password(tokenEnv: env))
            async let tb = CloudBridgeTransport.connect(
                channel: chB, server: true, certDER: cert2, keyPKCS8: key2,
                approvedPeerPEMs: [pem1], localAgentId: "brB", localKernel: kernelB,
                ingress: IngressRules.Password(tokenEnv: env))
            let (transportA, transportB) = try await (ta, tb)
            // A→B: A presents the fleet token (egress), B's password accepts → echo works.
            let fwd = await transportA.forward(
                target: "echo", payload: ["type": "echo", "msg": "hi"])
            #expect(fwd["echoed"]["msg"].asString == "hi", "egress should present: \(fwd)")
            // B→A: A's ingress is deny_inbound → refused regardless of token.
            let rev = await transportB.forward(target: "core", payload: ["type": "list_agents"])
            #expect(rev["reason"].asString == "unauthorized", "hub refuses inbound: \(rev)")
            await transportA.close()
            await transportB.close()
        }
    }
}
