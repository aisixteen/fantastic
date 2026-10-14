// credential_store: secrets by ref, bound to a host, read by whoever the call gate
// admits. Twin of python
// `security/credential_store/tests/test_credential_store.py`, plus the swift pins
// (the file's bytes, the host normalization, the readme).
//
// Tests never touch the real keychain or home: the file backend writes under a
// temp dir, the keychain backend runs on an in-memory double, and the env backend
// reads variables the process already has (no `setenv` in a threaded test).

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticCredentialStore
@testable import FantasticKernel
@testable import FantasticKernelState

private let KEY = "sk-live-0123456789abcdef"
private let KEY2 = "sk-live-rotated-fedcba98"

/// An in-memory keychain — never the real one.
final class MemKeychain: KeychainStore, @unchecked Sendable {
    private let lock = NSLock()
    private var _items: [String: String] = [:]
    var items: [String: String] { lock.withLock { _items } }
    func get(service: String, account: String) throws -> String? {
        lock.withLock { _items["\(service)\u{0}\(account)"] }
    }
    func set(service: String, account: String, value: String) throws {
        lock.withLock { _items["\(service)\u{0}\(account)"] = value }
    }
    func delete(service: String, account: String) throws {
        lock.withLock { _ = _items.removeValue(forKey: "\(service)\u{0}\(account)") }
    }
}

/// A temp dir per test, removed by the caller.
private func tmpDir() -> URL {
    let d = FileManager.default.temporaryDirectory
        .appendingPathComponent("cred-\(UUID().uuidString.prefix(8))")
    try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

private struct Fixture {
    let k: Kernel
    let dir: URL
    var file: URL { dir.appendingPathComponent("creds.yaml") }
}

/// Python `_mk`: four bare callers, and a store whose file lives under the temp dir.
private func mk(
    keychain: any KeychainStore = MemKeychain(), _ meta: OrderedDictionary<String, JSON> = [:]
) async -> Fixture {
    let r = BundleRegistry()
    r.register("kernel_state.tools", KernelStateBundle())
    r.register(CredentialStoreBundle.handlerModule, CredentialStoreBundle(keychain: keychain))
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: "kernel_state.tools", parentId: nil)
    k.register(root)
    k.setRoot(root)
    k.update(root.id, ["guid": "g1"])
    for aid in ["alice", "bob", "eve", "maker"] { bare(k, aid) }
    let dir = tmpDir()
    var payload: OrderedDictionary<String, JSON> = [
        "parent_id": "kernel_state", "handler_module": .string(CredentialStoreBundle.handlerModule),
        "id": "creds",
        "callable_by": ["alice": "*", "bob": "*", "eve": "*", "maker": "*"],
        "file": .string(dir.appendingPathComponent("creds.yaml").path),
    ]
    for (key, v) in meta { payload[key] = v }
    let rec = await create(k, payload)
    #expect(rec["id"] == "creds", "\(rec.serialize())")
    return Fixture(k: k, dir: dir)
}

/// Python `k.create(None, id=…, can_call={"creds": "*"})`: a bare caller — a
/// record and nothing else (`create_agent` here wants a handler_module).
private func bare(_ k: Kernel, _ id: String, parent: String = "kernel_state") {
    k.register(
        Agent(
            id: AgentId(id), handlerModule: nil, parentId: AgentId(parent),
            meta: ["can_call": ["creds": "*"]]))
}

private func create(_ k: Kernel, _ fields: OrderedDictionary<String, JSON>) async -> JSON {
    var payload: OrderedDictionary<String, JSON> = ["type": "create_agent"]
    for (key, v) in fields { payload[key] = v }
    return await k.send([Item("kernel", .object(payload))])[0].result
}

/// Python `_call`: one item to the store, as the operator or AS an agent.
private func call(
    _ k: Kernel, _ payload: OrderedDictionary<String, JSON>, as sender: String? = nil,
    target: String = "creds"
) async -> Outcome {
    let it = Item(AgentId(target), .object(payload))
    if let sender {
        return await KernelTaskLocals.$sender.withValue(AgentId(sender)) {
            await k.send([it])[0]
        }
    }
    return await k.send([it])[0]
}

private func record(_ k: Kernel) -> OrderedDictionary<String, JSON> {
    k.agent(AgentId("creds"))?.meta ?? [:]
}

/// Python `_declare`: declare refs the ONE way there is — the operator's
/// `update_agent` on the store's `refs`. It replaces the map, so the refs already
/// there ride along.
private func declare(_ k: Kernel, _ refs: OrderedDictionary<String, JSON>) async {
    var all = record(k)["refs"]?.asObject ?? [:]
    for (ref, entry) in refs { all[ref] = entry }
    let out = await k.send([
        Item("kernel", .object(["type": "update_agent", "id": "creds", "refs": .object(all)]))
    ])[0].result
    #expect(out["updated"] == .bool(true), "\(out.serialize())")
}

/// Python `_seed`: the operator declares `openai/main`, then stores its value.
@discardableResult
private func seed(_ k: Kernel) async -> Outcome {
    await declare(
        k, ["openai/main": ["host": "api.openai.com"]])
    let out = await call(
        k,
        [
            "type": "set", "ref": "openai/main",
            "fields": ["api_key": .string(KEY)],
        ])
    #expect(out.status == .completed, "\(out.asJSON.serialize())")
    return out
}

/// Python `_relative`: the refusal for a `file` that is not absolute.
private func relative(_ p: String) -> String {
    "file='\(p)' must be an absolute path (starting with / or ~) — a relative path is refused"
}

private func ledger(_ k: Kernel) async -> JSON {
    await k.send([Item("kernel", ["type": "ledger"])])[0].result
}

private func mode(_ url: URL) -> Int {
    var st = stat()
    _ = lstat(url.path, &st)
    return Int(st.st_mode) & 0o777
}

@Suite("credential_store")
struct CredentialStoreTests {

    // ─── the file backend ───────────────────────────────────────────────────

    @Test func setStoresTheValue0600AndTheRecordKeepsOnlyMetadata() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let out = await seed(f.k)
        #expect(
            out.result
                == .object([
                    "ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"],
                    "stored": true,
                ]), "\(out.result.serialize())")
        #expect(mode(f.file) == 0o600)
        #expect(try String(contentsOf: f.file, encoding: .utf8).contains(KEY))
        let rec = JSON.object(record(f.k))
        #expect(!rec.serialize().contains(KEY))
        #expect(rec["refs"]["openai/main"]["host"] == "api.openai.com")
        #expect(rec["refs"]["openai/main"]["fields"] == ["api_key"])
    }

    /// No per-ref access lists: whoever the call gate admits may `get`/`set` a
    /// declared ref; a caller the store's `callable_by` does not name never
    /// reaches the handler.
    @Test func anyAdmittedCallerReadsAndRotatesAStrangerIsGated() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let a = await call(f.k, ["type": "get", "ref": "openai/main"], as: "alice")
        #expect(a.result["fields"] == ["api_key": .string(KEY)], "\(a.result.serialize())")
        let r = await call(
            f.k, ["type": "set", "ref": "openai/main", "fields": ["api_key": .string(KEY2)]],
            as: "alice")
        #expect(r.result["stored"] == true, "\(r.result.serialize())")
        let b = await call(f.k, ["type": "get", "ref": "openai/main"], as: "bob")
        #expect(b.result["fields"] == ["api_key": .string(KEY2)], "\(b.result.serialize())")
        bare(f.k, "mallory")
        let m = await call(f.k, ["type": "get", "ref": "openai/main"], as: "mallory")
        #expect(m.result["reason"] == "unauthorized", "\(m.result.serialize())")
        #expect(m.result["side"] == "ingress", "\(m.result.serialize())")
        // list is metadata only
        let listed = (await call(f.k, ["type": "list"], as: "alice")).result["refs"]
        #expect(
            listed
                == .array([
                    .object(["ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"]])
                ]), "\(listed.serialize())")
        #expect(!listed.serialize().contains(KEY2))
    }

    /// A ref is declared ONLY by `update_agent` on the store's `refs`. `set` on an
    /// undeclared ref answers "no such ref" — to an agent AND to the operator —
    /// and stores nothing; once declared, its writer stores and its reader reads.
    @Test func setNeverCreatesARefUpdateAgentDeclaresIt() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let put: OrderedDictionary<String, JSON> = [
            "type": "set", "ref": "mine", "fields": ["token": .string(KEY)],
        ]
        for who in [nil, "maker"] as [String?] {
            let nope = await call(f.k, put, as: who)
            #expect(
                nope.result
                    == .object([
                        "error": "credential_store: no such ref 'mine'", "reason": "not_found",
                        "arg": "ref",
                    ]), "\(who ?? "operator"): \(nope.asJSON.serialize())")
        }
        #expect((record(f.k)["refs"]?.asObject ?? [:])["mine"] == nil)
        #expect(
            !FileManager.default.fileExists(atPath: f.file.path), "an undeclared set stores nothing")
        await declare(f.k, ["mine": ["host": "x.example"]])
        let made = await call(f.k, put, as: "maker")
        #expect(made.result["stored"] == true, "\(made.result.serialize())")
        let got = await call(f.k, ["type": "get", "ref": "mine"], as: "maker")
        #expect(got.result["fields"] == ["token": .string(KEY)], "\(got.result.serialize())")
        let entry = record(f.k)["refs"]?["mine"] ?? .null
        #expect(
            entry.asObject.map { Array($0.keys) } == ["host", "fields"],
            "a set keeps the declaration's keys in place and adds the field names")
        var theirs = put
        theirs["ref"] = "theirs"
        let nope = await call(f.k, theirs, as: "eve")
        #expect(nope.result["reason"] == "not_found", "\(nope.result.serialize())")
    }

    /// A ref's host is its DECLARATION — written by
    /// `update_agent` on the store's `refs`, the one path. `set` stores the value
    /// of a declared ref and takes no host: a writer naming one is refused, loud,
    /// with nothing stored and the declared host untouched, so a writer can never
    /// rebind a ref (and deny every reader at the real host).
    @Test func setTakesNoHostTheDeclarationIsTheBinding() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let rebind = await call(
            f.k,
            [
                "type": "set", "ref": "openai/main", "host": "attacker.example",
                "fields": ["api_key": .string(KEY2)],
            ], as: "bob")
        #expect(
            rebind.result
                == .object([
                    "error": .string(
                        "credential_store set: takes no host — a ref's host is its declaration, "
                        + "written by update_agent on this store's refs"),
                    "reason": "bad_arg", "arg": "host",
                ]), "\(rebind.result.serialize())")
        #expect(record(f.k)["refs"]?["openai/main"]["host"] == "api.openai.com")
        #expect(!(try String(contentsOf: f.file, encoding: .utf8)).contains(KEY2), "nothing stored")
        // the operator too: the host moves only with the declaration
        let op = await call(
            f.k,
            [
                "type": "set", "ref": "openai/main", "host": "api.openai.com",
                "fields": ["api_key": .string(KEY2)],
            ])
        #expect(op.result["arg"] == "host", "\(op.result.serialize())")
        // a set with no host stores the value under the declared host
        let ok = await call(
            f.k, ["type": "set", "ref": "openai/main", "fields": ["api_key": .string(KEY2)]],
            as: "bob")
        #expect(
            ok.result
                == .object([
                    "ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"],
                    "stored": true,
                ]), "\(ok.result.serialize())")
        let got = await call(
            f.k, ["type": "get", "ref": "openai/main", "host": "https://api.openai.com/v1"],
            as: "alice")
        #expect(got.result["fields"] == ["api_key": .string(KEY2)], "\(got.result.serialize())")
    }

    /// The host is the binding, so a declaration without one is refused at the
    /// write: a value stored under it could be sent nowhere — or anywhere.
    @Test func aRefIsDeclaredWithItsHost() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let r = await f.k.send([
            Item(
                "kernel",
                .object([
                    "type": "update_agent", "id": "creds",
                    "refs": ["nohost": .object([:])],
                ]))
        ])[0].result
        #expect(
            r
                == .object([
                    "error":
                        "refs['nohost'].host is required — the one host this ref's value may be sent to",
                    "reason": "invalid_meta", "id": "creds",
                ]), "\(r.serialize())")
    }

    /// `forget` drops the VALUE — from the backend, with
    /// its field names — and nothing else. The declaration (host) is written and
    /// removed by `update_agent` alone, so a caller can never undeclare a ref
    /// nobody could then set again.
    @Test func forgetDropsTheValueAndKeepsTheDeclaration() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let declared = record(f.k)["refs"]?["openai/main"] ?? .null
        let out = await call(f.k, ["type": "forget", "ref": "openai/main"], as: "bob")
        #expect(out.result == ["forgotten": true, "ref": "openai/main"])
        #expect(!(try String(contentsOf: f.file, encoding: .utf8)).contains(KEY))
        #expect(mode(f.file) == 0o600, "a rewrite keeps the file 0600")
        let entry = record(f.k)["refs"]?["openai/main"] ?? .null
        #expect(
            entry == .object(["host": declared["host"]]), "\(entry.serialize())")
        // a reader now finds the ref declared and empty
        let empty = await call(
            f.k, ["type": "get", "ref": "openai/main"], as: "alice")
        #expect(
            empty.result
                == .object([
                    "error": "credential_store: ref 'openai/main' has no value in the backend",
                    "reason": "not_found", "arg": "ref",
                ]), "\(empty.result.serialize())")
        // its writer sets it again, and the reader reads the new value
        let back = await call(
            f.k, ["type": "set", "ref": "openai/main", "fields": ["api_key": .string(KEY2)]],
            as: "bob")
        #expect(back.result["stored"] == true, "\(back.asJSON.serialize())")
        let got = await call(
            f.k, ["type": "get", "ref": "openai/main"], as: "alice")
        #expect(got.result["fields"] == ["api_key": .string(KEY2)], "\(got.result.serialize())")
        // an undeclared ref is not found; undeclaring is update_agent's alone
        let nope = await call(f.k, ["type": "forget", "ref": "never/declared"], as: "eve")
        #expect(nope.result["reason"] == "not_found", "\(nope.result.serialize())")
    }

    /// The credential file is opened ONCE (`O_NOFOLLOW`)
    /// and the open descriptor is what is judged and read. Python injects the
    /// swap between a path check and the read; swift has no hook for that race,
    /// so this pins the other half — a symlink in the leaf's place is refused
    /// outright, nothing read — and `fsReadPrivate` itself is pinned in
    /// `FsModeTests`.
    @Test func theFileIsJudgedOnTheOneOpenItIsReadFrom() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let planted = f.dir.appendingPathComponent("planted.yaml")
        try "refs:\n  openai/main:\n    fields:\n      api_key: sk-PLANTED\n    host: api.openai.com\n"
            .write(to: planted, atomically: true, encoding: .utf8)
        chmod(planted.path, 0o600)
        try FileManager.default.removeItem(at: f.file)
        symlink(planted.path, f.file.path)
        let out = await call(f.k, ["type": "get", "ref": "openai/main"])
        #expect(out.status == .failed, "\(out.asJSON.serialize())")
        #expect(
            (out.result["error"].asString ?? "").contains(
                "must be a regular file owned by this user with mode 0600"),
            "\(out.result.serialize())")
        #expect(!out.asJSON.serialize().contains("sk-PLANTED"))
    }

    /// The env backend is read-only: its value is the variable, so `forget` —
    /// which drops only a value — has nothing it may drop, and says so.
    @Test func forgetOnTheEnvBackendIsRefused() async throws {
        let f = await mk([
            "backend": "env",
            "refs": ["ci/key": ["host": "api.example", "env": ["api_key": "CI_TEST_KEY"]]],
        ])
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let out = await call(f.k, ["type": "forget", "ref": "ci/key"])
        #expect(
            out.result
                == .object([
                    "error": .string(
                        "credential_store forget: the env backend is read-only — unset the "
                        + "environment variable its ref names"),
                    "reason": "bad_arg", "arg": "backend",
                ]), "\(out.result.serialize())")
        #expect(record(f.k)["refs"]?["ci/key"] != nil)
    }

    @Test func aFileOthersCanReadIsRefusedLoud() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        chmod(f.file.path, 0o644)
        let out = await call(f.k, ["type": "get", "ref": "openai/main"])
        #expect(out.status == .failed, "\(out.asJSON.serialize())")
        let err = out.result["error"].asString ?? ""
        #expect(err.contains("mode 0600") && err.contains("0o644"), "\(err)")
        #expect(
            err
                == "PermissionError: credential_store: credential file \(f.file.path) must be a "
                + "regular file owned by this user with mode 0600 (is 0o644, uid \(getuid())) — "
                + "refused, nothing read")
        #expect(out.result["raised"] == true)
    }

    /// The file's bytes are python's — the kernel's YAML emitter with every map's
    /// keys sorted, each ref's entry `{fields, host}` (the host stored WITH the
    /// value) — either runtime reads the other's file.
    @Test func theFileIsPythonsBytesKeysSorted() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await declare(f.k, ["z/ref": ["host": "h.example"], "openai/main": ["host": "h.example"]])
        for (ref, fields) in [
            ("z/ref", ["token": JSON.string("tok-1"), "api_key": .string(KEY)]),
            ("openai/main", ["api_key": JSON.string("sk-x")]),
        ] as [(String, OrderedDictionary<String, JSON>)] {
            let out = await call(
                f.k, ["type": "set", "ref": .string(ref), "fields": .object(fields)])
            #expect(out.status == .completed, "\(out.asJSON.serialize())")
        }
        #expect(
            try String(contentsOf: f.file, encoding: .utf8)
                == "refs:\n  openai/main:\n    fields:\n      api_key: sk-x\n    host: h.example\n"
                + "  z/ref:\n    fields:\n      api_key: \(KEY)\n      token: tok-1\n"
                + "    host: h.example\n")
        // the reply names the fields sorted
        let back = await call(f.k, ["type": "get", "ref": "z/ref"])
        #expect(back.result["fields"].asObject.map { Array($0.keys) } == ["api_key", "token"])
        // forgetting both leaves python's empty map
        _ = await call(f.k, ["type": "forget", "ref": "z/ref"])
        _ = await call(f.k, ["type": "forget", "ref": "openai/main"])
        #expect(try String(contentsOf: f.file, encoding: .utf8) == "refs: {}\n")
    }

    /// `file` is `/…` or `~…` — never relative to the kernel's running dir. A
    /// relative one is refused AT THE WRITE (create and update alike), and a
    /// record that carries one past the write (an in-process register) is refused
    /// at use, loud, with nothing read or written.
    @Test func theFileIsAnAbsolutePathARelativeOneIsRefused() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = BundleRegistry()
        r.register("kernel_state.tools", KernelStateBundle())
        r.register(CredentialStoreBundle.handlerModule, CredentialStoreBundle(keychain: MemKeychain()))
        let k = Kernel(storage: .inMemory, bundles: r)
        let root = Agent(
            id: AgentId("kernel_state"), handlerModule: "kernel_state.tools", parentId: nil,
            rootPath: dir.appendingPathComponent(".fantastic"))
        k.register(root)
        k.setRoot(root)
        for rel in ["creds.yaml", "sub/creds.yaml", "./creds.yaml"] {
            let made = await create(k, [
                "parent_id": "kernel_state", "handler_module": "credential_store.tools", "id": "rel",
                "file": .string(rel),
            ])
            #expect(
                made == .object(["error": .string(relative(rel)), "reason": "invalid_meta"]),
                "\(made.serialize())")
            #expect(k.agent(AgentId("rel")) == nil)
        }
        let abs = dir.appendingPathComponent("creds.yaml").path
        let made = await create(k, [
            "parent_id": "kernel_state", "handler_module": "credential_store.tools", "id": "creds",
            "file": .string(abs),
        ])
        #expect(made["id"] == "creds", "\(made.serialize())")
        let upd = await k.send([
            Item("kernel", .object(["type": "update_agent", "id": "creds", "file": "creds.yaml"]))
        ])[0].result
        #expect(
            upd
                == .object([
                    "error": .string(relative("creds.yaml")), "reason": "invalid_meta", "id": "creds",
                ]), "\(upd.serialize())")
        #expect(k.agent(AgentId("creds"))?.meta["file"] == .string(abs))
        // `~…` passes the write (no disk is touched to judge it)
        let b = CredentialStoreBundle(keychain: MemKeychain())
        #expect(b.validateMeta(["file": "~/.fantastic/credentials/x.yaml"], record: [:]) == nil)
        #expect(
            b.validateMeta(["keychain_namespace": ""], record: [:])
                == "keychain_namespace must be a non-empty string")
        // past the write: refused at use, nothing written under the running dir
        k.register(
            Agent(
                id: AgentId("sneaky"), handlerModule: CredentialStoreBundle.handlerModule,
                parentId: AgentId("kernel_state"),
                meta: ["file": "creds.yaml", "refs": ["r": ["host": "h"]]]))
        let out = await call(
            k, ["type": "set", "ref": "r", "fields": ["api_key": .string(KEY)]], target: "sneaky")
        #expect(out.status == .failed, "\(out.asJSON.serialize())")
        #expect(
            out.result["error"]
                == .string("ValueError: credential_store: \(relative("creds.yaml"))"),
            "\(out.asJSON.serialize())")
        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent("creds.yaml").path))
        // the reflect still names the file as written — it reads no disk
        let refl = await call(k, ["type": "reflect"], target: "sneaky")
        #expect(refl.result["file"] == "creds.yaml" && refl.result["backend"] == "file")
    }

    // ─── the keychain backend (an in-memory double — never the real one) ────

    @Test func theKeychainBackendKeepsOneItemPerRef() async throws {
        let ring = MemKeychain()
        let f = await mk(keychain: ring, ["backend": "keychain"])
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        // the item holds the declared host WITH the value
        #expect(
            ring.items == [
                "fantastic\u{0}creds/openai/main":
                    #"{"host": "api.openai.com", "fields": {"api_key": "\#(KEY)"}}"#
            ])
        #expect(!FileManager.default.fileExists(atPath: f.file.path), "no file is written")
        let got = await call(f.k, ["type": "get", "ref": "openai/main"])
        #expect(got.result["fields"] == ["api_key": .string(KEY)])
        _ = await call(f.k, ["type": "forget", "ref": "openai/main"])
        #expect(ring.items.isEmpty)
        // reflect: no file for a keychain store
        let refl = await call(f.k, ["type": "reflect"])
        #expect(refl.result.asObject?["file"] == nil && refl.result["backend"] == "keychain")
    }

    /// One item per ref, under `keychain_namespace` (default `fantastic` — the
    /// keychain's own word for it is the item's "service"), account `<id>/<ref>`.
    @Test func theKeychainNamespaceIsTheRecords() async throws {
        let ring = MemKeychain()
        let f = await mk(keychain: ring, ["backend": "keychain", "keychain_namespace": "acme"])
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        #expect(Array(ring.items.keys) == ["acme\u{0}creds/openai/main"])
    }

    // ─── the env backend (read-only) ────────────────────────────────────────

    @Test func theEnvBackendReadsNamedVariablesAndRefusesWrites() async throws {
        // a variable the process already has — no `setenv` in a threaded test
        let path = try #require(ProcessInfo.processInfo.environment["PATH"])
        let unset = "FANTASTIC_CRED_TEST_UNSET_\(UUID().uuidString.prefix(8))"
        let f = await mk([
            "backend": "env",
            "refs": [
                "ci/key": [
                    "host": "api.example", "env": ["api_key": "PATH"],
                ],
                "ci/gone": [
                    "host": "api.example", "env": ["api_key": .string(unset)],
                ],
            ],
        ])
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let got = await call(f.k, ["type": "get", "ref": "ci/key"], as: "alice")
        #expect(
            got.result
                == .object([
                    "ref": "ci/key", "host": "api.example", "fields": ["api_key": .string(path)],
                ]), "\(got.result.serialize())")
        let w = await call(
            f.k,
            [
                "type": "set", "ref": "ci/key",
                "fields": ["api_key": .string(KEY2)],
            ])
        #expect(w.result["arg"] == "backend", "\(w.result.serialize())")
        let gone = await call(f.k, ["type": "get", "ref": "ci/gone"], as: "alice")
        #expect(gone.status == .failed, "\(gone.asJSON.serialize())")
        #expect(
            gone.result["error"]
                == .string(
                    "LookupError: credential_store: ref 'ci/gone' reads 'api_key' from $\(unset), "
                        + "which is unset"))
        // the listing shows the env field names
        let listed = (await call(f.k, ["type": "list"], as: "alice")).result["refs"]
        #expect(listed[0]["fields"] == ["api_key"])
    }

    @Test func aBadRecordIsRefusedAtTheWrite() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let r = await create(f.k, [
            "parent_id": "kernel_state", "handler_module": "credential_store.tools", "id": "bad",
            "backend": "vault",
        ])
        #expect(r["reason"] == "invalid_meta", "\(r.serialize())")
        #expect(r["error"].asString?.contains("backend") == true, "\(r.serialize())")
        // python's texts
        let b = CredentialStoreBundle(keychain: MemKeychain())
        let cases: [(OrderedDictionary<String, JSON>, String)] = [
            (["backend": "vault"], "backend='vault' is not one of file, keychain, env"),
            (["file": "  "], "file must be a non-empty string"),
            (["file": "creds.yaml"], relative("creds.yaml")),
            (["file": " /x.yaml"], relative(" /x.yaml")),
            (["keychain_namespace": 5], "keychain_namespace must be a non-empty string"),
            (["refs": ["a", "b"]], "refs must be a map of ref to {host, fields, env}"),
            (["refs": [" ": [:]]], "a ref must be a non-empty string"),
            (["refs": ["r": "x"]], "refs['r'] must be a map"),
            (["refs": ["r": ["fields": "api_key"]]], "refs['r'].fields must be a list of strings"),
            (["refs": ["r": ["host": "  "]]], "refs['r'].host must be a non-empty host"),
            (["refs": ["r": .object([:])]], "refs['r'].host is required — the one host this ref's value may be sent to"),
            (["refs": ["r": ["host": "h", "env": ["api_key": ""]]]], "refs['r'].env must map field names to variable names"),
        ]
        for (meta, text) in cases {
            #expect(b.validateMeta(meta, record: [:]) == text, "\(meta)")
        }
        #expect(b.validateMeta(["backend": .null, "refs": ["r": ["host": "https://h/x"]]], record: [:]) == nil)
    }

    // ─── the verbs' own refusals ────────────────────────────────────────────

    @Test func theArgumentRefusalsArePythons() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let noRef = await call(f.k, ["type": "set", "fields": ["k": "v"]])
        #expect(
            noRef.result
                == .object([
                    "error": "credential_store set: ref required", "reason": "missing_arg",
                    "arg": "ref",
                ]))
        // a `host` on a set is refused before anything else is judged (AV-4)
        let withHost = await call(f.k, ["type": "set", "ref": "r", "host": "://x", "fields": ["k": "v"]])
        #expect(
            withHost.result
                == .object([
                    "error": .string(
                        "credential_store set: takes no host — a ref's host is its declaration, "
                        + "written by update_agent on this store's refs"),
                    "reason": "bad_arg", "arg": "host",
                ]), "\(withHost.result.serialize())")
        let badFields = await call(f.k, ["type": "set", "ref": "r", "fields": ["k": ""]])
        #expect(
            badFields.result["error"]
                == "credential_store set: fields must be a non-empty map of field name to string value")
        #expect(badFields.result["arg"] == "fields")
        // each field marked one by one is a mark too
        await declare(f.k, ["r": ["host": "h"]])
        let perField = await call(
            f.k, ["type": "set", "ref": "r", "fields": ["a": "1", "b": "2"]])
        #expect(perField.result["stored"] == true, "\(perField.result.serialize())")
        let getNoRef = await call(f.k, ["type": "get"])
        #expect(getNoRef.result["error"] == "credential_store get: ref required")
        let forgetNoRef = await call(f.k, ["type": "forget"])
        #expect(forgetNoRef.result["error"] == "credential_store forget: ref required")
        let unknown = await call(f.k, ["type": "nope"])
        #expect(
            unknown.result
                == .object([
                    "error": "credential_store: unknown type 'nope'", "reason": "unhandled",
                    "verb": "nope",
                ]))
        // a field-level mark on the answer
        let fieldMark = await call(
            f.k, ["type": "get", "ref": "r"])
        #expect(fieldMark.result["fields"] == ["a": "1", "b": "2"], "\(fieldMark.result.serialize())")
    }

    @Test func aReadPinnedToAnotherHostIsRefused() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let evil = await call(
            f.k, ["type": "get", "ref": "openai/main", "host": "https://evil.example.net/v1"])
        #expect(
            evil.result
                == .object([
                    "error": .string(
                        "credential_store: ref 'openai/main' is bound to host 'api.openai.com', not "
                            + "'evil.example.net' — refused, nothing read"),
                    "reason": "bad_arg", "arg": "host",
                ]), "\(evil.result.serialize())")
        let ok = await call(
            f.k, ["type": "get", "ref": "openai/main", "host": "https://API.openai.com/v1/responses"])
        #expect(ok.result["fields"] == ["api_key": .string(KEY)], "\(ok.result.serialize())")
    }

    // ─── the host at rest ────────────────────────────

    /// Python `_at_rest`.
    private func atRest(_ ref: String, _ stored: String, _ declared: String) -> JSON {
        .object([
            "error": .string(
                "credential_store: ref '\(ref)' is bound to host '\(declared)', not "
                    + "'\(stored)' — refused, nothing read"),
            // the same answer as `get {ref, host}` naming another host
            "reason": "bad_arg",
            "arg": "host",
        ])
    }

    /// The value is stored WITH the host its declaration named at `set`. The
    /// operator re-declaring the ref with another host does not carry the value
    /// there: every `get` is refused, with or without a `host`, and nothing is read
    /// — until a writer sets the value again, which stores it under the new host.
    @Test func aRedeclaredHostReadsNothingUntilTheValueIsSetAgain() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        var entry = record(f.k)["refs"]?["openai/main"].asObject ?? [:]
        entry["host"] = "attacker.example"
        await declare(f.k, ["openai/main": .object(entry)])
        for payload: OrderedDictionary<String, JSON> in [
            ["type": "get", "ref": "openai/main"],
            ["type": "get", "ref": "openai/main", "host": "attacker.example"],
        ] {
            let out = await call(f.k, payload, as: "alice")
            #expect(
                out.result == atRest("openai/main", "api.openai.com", "attacker.example"),
                "\(out.result.serialize())")
            #expect(!out.asJSON.serialize().contains(KEY))
        }
        let back = await call(
            f.k, ["type": "set", "ref": "openai/main", "fields": ["api_key": .string(KEY2)]],
            as: "bob")
        #expect(back.result["host"] == "attacker.example", "\(back.result.serialize())")
        let got = await call(f.k, ["type": "get", "ref": "openai/main"], as: "alice")
        #expect(
            got.result
                == .object([
                    "ref": "openai/main", "host": "attacker.example",
                    "fields": ["api_key": .string(KEY2)],
                ]), "\(got.result.serialize())")
    }

    /// S3, the shared-default case: a second store naming the same `file` declares
    /// the SAME ref with another host. The value was stored for the first store's
    /// host, so the second store's `get` is refused — the operator's included.
    @Test func anotherStoreSharingTheFileReadsNothingForItsHost() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let made = await create(
            f.k,
            [
                "parent_id": "kernel_state",
                "handler_module": .string(CredentialStoreBundle.handlerModule), "id": "other",
                "file": .string(f.file.path),
                "refs": ["openai/main": ["host": "blocked.invalid"]],
            ])
        #expect(made["id"] == "other", "\(made.serialize())")
        let out = await call(
            f.k, ["type": "get", "ref": "openai/main", "host": "blocked.invalid"], target: "other")
        #expect(
            out.result == atRest("openai/main", "api.openai.com", "blocked.invalid"),
            "\(out.result.serialize())")
        #expect(!out.asJSON.serialize().contains(KEY))
    }

    /// No back-compat: a value at rest that carries no host — a bare
    /// `{field: value}` shape, or a hand-written entry — is refused, nothing read.
    @Test(arguments: ["file", "keychain"])
    func aStoredValueWithoutAHostIsRefused(backend: String) async throws {
        let ring = MemKeychain()
        let f = await mk(keychain: ring, ["backend": .string(backend)])
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await declare(f.k, ["openai/main": ["host": "api.openai.com"]])
        if backend == "file" {
            try "refs:\n  openai/main:\n    api_key: \(KEY)\n".write(
                to: f.file, atomically: true, encoding: .utf8)
            chmod(f.file.path, 0o600)
        } else {
            try ring.set(
                service: "fantastic", account: "creds/openai/main", value: #"{"api_key": "\#(KEY)"}"#)
        }
        let out = await call(f.k, ["type": "get", "ref": "openai/main"], as: "alice")
        #expect(
            out.result
                == .object([
                    "error": .string(
                        "credential_store: ref 'openai/main' is bound to host 'api.openai.com', "
                            + "not None — refused, nothing read"),
                    "reason": "bad_arg",
                    "arg": "host",
                ]), "\(out.result.serialize())")
        #expect(!out.asJSON.serialize().contains(KEY))
    }

    @Test func theReflectIsPostureNeverAValue() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let r = await call(f.k, ["type": "reflect"])
        #expect(r.result["backend"] == "file" && r.result["refs"] == 1)
        #expect(r.result["file"] == .string(f.file.path))
        #expect(r.result.asObject?["verbs"] == nil)
        #expect(
            CredentialStoreBundle().verbDocTable.asObject.map { Array($0.keys) }
                == ["reflect", "boot", "set", "get", "list", "forget"])
        #expect(!r.result.serialize().contains(KEY) && !r.result.serialize().contains("openai/main"))
    }

    // ─── the host, as stored and compared (python `urlsplit(h).netloc`) ─────

    @Test func theHostIsUrlsplitsNetlocLowercased() throws {
        let cases: [(String, String?)] = [
            ("https://API.OpenAI.com:443/v1", "api.openai.com:443"),
            ("ws://h/x?q", "h"),
            ("://x", nil),
            ("1http://x", nil),
            ("api.example", "api.example"),
            ("  Api.Example  ", "api.example"),
            ("https://u:p@Host/", "u:p@host"),
            ("", nil),
        ]
        for (raw, want) in cases {
            #expect(try hostOf(.string(raw)) == want, "\(raw)")
        }
        #expect(throws: PyError.self) { try hostOf(.string("http://[::1/x")) }
    }

    /// The readme is python's `readme.md`, byte for byte.
    @Test func theReadmeIsPythonsVerbatim() throws {
        let py = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent(
                "python/bundled_agents/security/credential_store/src/credential_store/readme.md")
        #expect(CredentialStoreBundle().readme == (try String(contentsOf: py, encoding: .utf8)))
    }
}
