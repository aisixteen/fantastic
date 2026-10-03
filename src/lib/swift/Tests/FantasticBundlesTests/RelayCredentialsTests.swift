// The relay password lives in a credential_store — never on the record.
//
// Audit IO-7 / F4 (2026-10-03): `relay_token` (the relay group password, sent as
// `X-Fantastic-Auth`) was a plain record field — in `list_agents`, and in
// cleartext in `.fantastic/agents/<id>/agent.yaml`. Owner rule: secrets live only
// in a credential_store. The connector's record names WHERE the password lives —
// `credentials: {store, ref}` — and before every dial it reads the ref's
// `password` field AS ITSELF (the dial runs in its own dispatch, or a task it
// spawned), pinned to the relay URL's host, exactly as an AI backend reads its
// key. `relay_token` is gone.
//
// Python stubs the network dial (`_open_ws`) and records its headers; here a
// loopback listener records the upgrade request the real dial sends, then hangs
// up (so the one-shot boot fails AFTER the dial — the header is what is pinned).
//
// Twin of python `relay_connector/tests/test_relay_credentials.py`.

import FantasticCredentialStore
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import FantasticKernelState
import Foundation
import OrderedCollections
import Testing

#if canImport(Darwin)
    import Darwin
#else
    import Glibc
#endif

private let PW = "relay-group-pw-SECRET-9f9f"
private let PW2 = "relay-group-pw-ROTATED-0e0e"

/// A loopback listener that records every request head a client sends, then
/// closes the connection.
private final class DialCapture: @unchecked Sendable {
    let port: UInt16
    private let fd: Int32
    private let lock = NSLock()
    private var heads: [String] = []
    private var stopped = false

    init() {
        let s = socket(AF_INET, Int32(SOCK_STREAM), 0)
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = 0
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        _ = withUnsafePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(s, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        listen(s, 8)
        var out = sockaddr_in()
        var len = socklen_t(MemoryLayout<sockaddr_in>.size)
        _ = withUnsafeMutablePointer(to: &out) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(s, $0, &len) }
        }
        fd = s
        port = UInt16(bigEndian: out.sin_port)
        let t = Thread { [self] in serve() }
        t.start()
    }

    private func serve() {
        while !lock.withLock({ stopped }) {
            var pfd = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
            if poll(&pfd, 1, 50) <= 0 { continue }
            let c = accept(fd, nil, nil)
            if c < 0 { continue }
            var tv = timeval(tv_sec: 2, tv_usec: 0)
            setsockopt(c, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
            var got = Data()
            var buf = [UInt8](repeating: 0, count: 4096)
            while !String(decoding: got, as: UTF8.self).contains("\r\n\r\n") {
                let n = recv(c, &buf, buf.count, 0)
                if n <= 0 { break }
                got.append(contentsOf: buf[0..<n])
            }
            lock.withLock { heads.append(String(decoding: got, as: UTF8.self)) }
            close(c)
        }
        close(fd)
    }

    /// Every request head so far.
    var requests: [String] { lock.withLock { heads } }

    /// The `X-Fantastic-Auth` value of each dial, in order.
    var authTokens: [String] {
        requests.map { head in
            head.components(separatedBy: "\r\n")
                .first { $0.lowercased().hasPrefix("x-fantastic-auth:") }
                .map { String($0.dropFirst("x-fantastic-auth:".count)).trimmingCharacters(in: .whitespaces) }
                ?? ""
        }
    }

    func stop() { lock.withLock { stopped = true } }
}

/// The keychain is never reached (file backend).
private struct NoKeychain: KeychainStore {
    func get(service: String, account: String) throws -> String? { nil }
    func set(service: String, account: String, value: String) throws {}
    func delete(service: String, account: String) throws {}
}

private func op(_ k: Kernel, _ target: String, _ payload: JSON) async -> Outcome {
    await k.send([Item(AgentId(target), payload)])[0]
}

/// Python fixture `relay_kernel`: a credential store holding the relay password
/// (ref `relay/group`, bound to `host`, readable by the leg `rl`).
private func relayKernel(host: String) async -> (Kernel, URL) {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("relaycred-\(UUID().uuidString.prefix(8))")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let r = BundleRegistry()
    r.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    r.register(CredentialStoreBundle.handlerModule, CredentialStoreBundle(keychain: NoKeychain()))
    r.register("relay_connector.tools", KernelBridgeBundle(family: .relay))
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: ROOT_HANDLER_MODULE, parentId: nil)
    k.register(root)
    k.setRoot(root)
    k.update(root.id, ["guid": "local-g"])
    let made = await op(
        k, "kernel",
        .object([
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": .string(CredentialStoreBundle.handlerModule), "id": "creds",
            "file": .string(dir.appendingPathComponent("creds.yaml").path),
            "refs": ["relay/group": ["host": .string(host)]],
            "callable_by": ["rl": ["get"]],
        ]))
    #expect(made.result["id"] == "creds", "\(made.result.serialize())")
    return (k, dir)
}

private func setPassword(_ k: Kernel, _ value: String) async {
    let out = await op(
        k, "creds",
        ["type": "set", "ref": "relay/group", "fields": ["password": .string(value)]])
    #expect(out.result["stored"] == true, "\(out.asJSON.serialize())")
}

private func leg(_ k: Kernel, url: String, _ meta: OrderedDictionary<String, JSON> = [:]) async
    -> JSON
{
    var p: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": "relay_connector.tools", "id": "rl", "transport": "relay",
        "relay_url": .string(url), "guid": "A", "partner_guid": "B", "heartbeat": 0,
        "reconnect": 0, "can_call": ["creds": ["get"]],
    ]
    for (key, v) in meta { p[key] = v }
    return await op(k, "kernel", .object(p)).result
}

private let CREDS: JSON = ["store": "creds", "ref": "relay/group"]

@Suite("relay_connector — the password lives in a credential_store", .serialized)
struct RelayCredentialsTests {

    @Test(.timeLimit(.minutes(1)))
    func thePasswordIsReadFromTheStoreAndNeverRecorded() async throws {
        let cap = DialCapture()
        defer { cap.stop() }
        let (k, dir) = await relayKernel(host: "127.0.0.1:\(cap.port)")
        defer { try? FileManager.default.removeItem(at: dir) }
        await setPassword(k, PW)
        let made = await leg(k, url: "ws://127.0.0.1:\(cap.port)", ["credentials": CREDS])
        #expect(made["id"] == "rl", "\(made.serialize())")
        _ = await op(k, "rl", ["type": "boot"])
        // create_agent boots the leg too; the listener hangs up, so the explicit
        // boot dials again — EVERY dial carries the stored password
        #expect(!cap.authTokens.isEmpty && cap.authTokens.allSatisfy { $0 == PW }, "\(cap.requests)")
        #expect((cap.requests.first ?? "").hasPrefix("GET /A HTTP/1.1"), "\(cap.requests)")
        // no record, listing or ledger carries it
        let record = JSON.object(k.agent(AgentId("rl"))?.meta ?? [:])
        #expect(!record.serialize().contains(PW))
        let listed = await op(k, "kernel", ["type": "list_agents"])
        #expect(!listed.asJSON.serialize().contains(PW))
        let ledger = await op(k, "kernel", ["type": "ledger"])
        #expect(!ledger.asJSON.serialize().contains(PW))
    }

    @Test(.timeLimit(.minutes(1)))
    func aRotatedPasswordIsUsedByTheNextDial() async throws {
        let cap = DialCapture()
        defer { cap.stop() }
        let (k, dir) = await relayKernel(host: "127.0.0.1:\(cap.port)")
        defer { try? FileManager.default.removeItem(at: dir) }
        await setPassword(k, PW)
        _ = await leg(k, url: "ws://127.0.0.1:\(cap.port)", ["credentials": CREDS])
        _ = await op(k, "rl", ["type": "boot"])
        await setPassword(k, PW2)
        _ = await op(k, "rl", ["type": "reconnect"])
        let tokens = cap.authTokens
        #expect(
            tokens.count >= 2 && tokens.last == PW2 && tokens.dropLast().allSatisfy { $0 == PW },
            "\(cap.requests)")
    }

    @Test(.timeLimit(.minutes(1)))
    func aLegWithNoCredentialsFailsBootLoud() async throws {
        let cap = DialCapture()
        defer { cap.stop() }
        let (k, dir) = await relayKernel(host: "127.0.0.1:\(cap.port)")
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await leg(k, url: "ws://127.0.0.1:\(cap.port)")
        let out = await op(k, "rl", ["type": "boot"])
        #expect(
            out.result
                == [
                    "error": .string(
                        "bridge: relay boot failed: relay_connector: no `credentials` on rl — name "
                            + "the relay password's credential store and ref (update_agent rl "
                            + "credentials={store: <id>, ref: <ref>}; the ref's field `password`, "
                            + "bound to the relay's host)")
                ], "\(out.result.serialize())")
        try await Task.sleep(nanoseconds: 100_000_000)
        #expect(cap.requests.isEmpty, "nothing dialed")
    }

    /// Pinned to the relay URL's host: a ref bound elsewhere is refused by the
    /// store, and the boot names the store, the ref and the store's own words.
    @Test(.timeLimit(.minutes(1)))
    func aStoreRefusalFailsTheDialNamingStoreAndRef() async throws {
        let (k, dir) = await relayKernel(host: "relay:9443")
        defer { try? FileManager.default.removeItem(at: dir) }
        await setPassword(k, PW)
        _ = await leg(k, url: "ws://elsewhere:1", ["credentials": CREDS])
        let out = await op(k, "rl", ["type": "boot"])
        #expect(
            out.result
                == [
                    "error": .string(
                        "bridge: relay boot failed: relay_connector: credentials "
                            + "creds/relay/group: credential_store: ref 'relay/group' is bound to "
                            + "host 'relay:9443', not 'elsewhere:1' — refused, nothing read")
                ], "\(out.result.serialize())")
        #expect(!out.asJSON.serialize().contains(PW))
    }

    @Test(arguments: [
        (JSON.string("creds"), "credentials must be {store: <credential_store id>, ref: <ref>}"),
        (JSON.object(["store": "creds"]), "credentials must be {store: <credential_store id>, ref: <ref>}"),
        (JSON.object(["store": "", "ref": "r"]), "credentials.store must be a non-empty string"),
    ] as [(JSON, String)])
    func aMalformedCredentialsFieldIsRefusedAtTheWrite(_ bad: JSON, _ why: String) async throws {
        let (k, dir) = await relayKernel(host: "relay:9443")
        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await leg(k, url: "ws://relay:9443", ["credentials": bad])
        #expect(out == ["error": .string(why), "reason": "invalid_meta"], "\(out.serialize())")
    }
}
