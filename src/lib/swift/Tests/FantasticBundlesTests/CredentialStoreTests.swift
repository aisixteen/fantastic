// credential_store: secrets by ref, bound to a host, read only with a secret mark,
// behind per-ref grants (owner, 2026-10-01). Twin of python
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
private let MARK_SET: JSON = .array([.string("payload.fields")])
private let MARK_GET: JSON = .array([.string("result")])

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
    secret: JSON? = nil, target: String = "creds"
) async -> Outcome {
    var it = Item(AgentId(target), .object(payload))
    it.secret = secret
    if let sender {
        return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
            await k.send([it])[0]
        }
    }
    return await k.send([it])[0]
}

private func record(_ k: Kernel) -> OrderedDictionary<String, JSON> {
    k.agent(AgentId("creds"))?.meta ?? [:]
}

/// Python `_seed`: the operator stores `openai/main`, then names alice its reader
/// and bob its writer.
@discardableResult
private func seed(_ k: Kernel) async -> Outcome {
    let out = await call(
        k,
        [
            "type": "set", "ref": "openai/main", "host": "https://api.openai.com/v1",
            "fields": ["api_key": .string(KEY)],
        ], secret: MARK_SET)
    #expect(out.status == .completed, "\(out.asJSON.serialize())")
    var refs = record(k)["refs"]?.asObject ?? [:]
    var entry = refs["openai/main"]?.asObject ?? [:]
    entry["readers"] = ["alice"]
    entry["writers"] = ["bob"]
    refs["openai/main"] = .object(entry)
    k.update(AgentId("creds"), ["refs": .object(refs)])
    return out
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

    @Test func valuesMoveOnlyWithASecretMark() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let unmarked = await call(
            f.k, ["type": "set", "ref": "r", "host": "h", "fields": ["api_key": .string(KEY)]])
        #expect(unmarked.result["reason"] == "bad_arg")
        #expect(unmarked.result["arg"] == "secret")
        #expect(
            unmarked.result["error"]
                == .string(
                    "credential_store set: mark the values secret — secret: ['payload.fields'] — "
                        + "or they would be recorded in the ledger, the memory store and the "
                        + "state stream"))
        #expect(!FileManager.default.fileExists(atPath: f.file.path), "an unmarked set stores nothing")
        await seed(f.k)
        let unmarkedGet = await call(f.k, ["type": "get", "ref": "openai/main"])
        #expect(unmarkedGet.result["arg"] == "secret", "\(unmarkedGet.result.serialize())")
        let got = await call(f.k, ["type": "get", "ref": "openai/main"], secret: MARK_GET)
        #expect(
            got.result
                == .object([
                    "ref": "openai/main", "host": "api.openai.com",
                    "fields": ["api_key": .string(KEY)],
                ]), "\(got.result.serialize())")
        #expect(!(await ledger(f.k)).serialize().contains(KEY), "the ledger must never hold the value")
    }

    @Test func readersReadWritersRotateStrangersSeeNothing() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        // a reader reads
        let a = await call(f.k, ["type": "get", "ref": "openai/main"], as: "alice", secret: MARK_GET)
        #expect(a.result["fields"] == ["api_key": .string(KEY)], "\(a.result.serialize())")
        // a reader may not write — told so, since it knows the ref
        let rotate: OrderedDictionary<String, JSON> = [
            "type": "set", "ref": "openai/main", "host": "api.openai.com",
            "fields": ["api_key": .string(KEY2)],
        ]
        let w = await call(f.k, rotate, as: "alice", secret: MARK_SET)
        #expect(w.result["reason"] == "unauthorized", "\(w.result.serialize())")
        #expect(
            w.result["error"] == "credential_store: alice may read 'openai/main' but not write it")
        // a writer rotates; the reader's next read gets the new value
        let r = await call(f.k, rotate, as: "bob", secret: MARK_SET)
        #expect(r.result["stored"] == true, "\(r.result.serialize())")
        let a2 = await call(f.k, ["type": "get", "ref": "openai/main"], as: "alice", secret: MARK_GET)
        #expect(a2.result["fields"] == ["api_key": .string(KEY2)])
        // a stranger is answered as if the ref did not exist
        let e = await call(f.k, ["type": "get", "ref": "openai/main"], as: "eve", secret: MARK_GET)
        #expect(
            e.result
                == .object([
                    "error": "credential_store: no such ref 'openai/main'", "reason": "not_found",
                    "arg": "ref",
                ]), "\(e.result.serialize())")
        #expect((await call(f.k, ["type": "list"], as: "eve")).result == ["refs": []])
        // list is metadata only
        let listed = (await call(f.k, ["type": "list"], as: "alice")).result["refs"]
        #expect(
            listed
                == .array([
                    .object([
                        "ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"],
                        "read": true, "write": false,
                    ])
                ]), "\(listed.serialize())")
        #expect(!listed.serialize().contains(KEY2))
        // the operator's listing names the grants too
        let op = (await call(f.k, ["type": "list"])).result["refs"][0]
        #expect(
            op.asObject.map { Array($0.keys) }
                == ["ref", "host", "fields", "read", "write", "readers", "writers"])
        #expect(op["readers"] == ["alice"] && op["writers"] == ["bob"])
    }

    @Test func aCreatorOwnsTheRefItMakesOthersCannotMakeOne() async throws {
        let f = await mk(["creators": ["maker"]])
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let made = await call(
            f.k,
            ["type": "set", "ref": "mine", "host": "x.example", "fields": ["token": .string(KEY)]],
            as: "maker", secret: MARK_SET)
        #expect(made.result["stored"] == true, "\(made.result.serialize())")
        let entry = record(f.k)["refs"]?["mine"] ?? .null
        #expect(entry["readers"] == ["maker"] && entry["writers"] == ["maker"])
        #expect(
            entry.asObject.map { Array($0.keys) } == ["readers", "writers", "host", "fields"])
        let nope = await call(
            f.k,
            ["type": "set", "ref": "theirs", "host": "x.example", "fields": ["token": .string(KEY)]],
            as: "eve", secret: MARK_SET)
        #expect(nope.result["reason"] == "not_found", "\(nope.result.serialize())")
    }

    @Test func aSubtreeGrantCoversDescendants() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        bare(f.k, "child", parent: "alice")
        var cb = record(f.k)["callable_by"]?.asObject ?? [:]
        cb["child"] = "*"
        var refs = record(f.k)["refs"]?.asObject ?? [:]
        var entry = refs["openai/main"]?.asObject ?? [:]
        entry["readers"] = ["$subtree:alice"]
        refs["openai/main"] = .object(entry)
        f.k.update(AgentId("creds"), ["refs": .object(refs), "callable_by": .object(cb)])
        let got = await call(f.k, ["type": "get", "ref": "openai/main"], as: "child", secret: MARK_GET)
        #expect(got.result["fields"] == ["api_key": .string(KEY)], "\(got.result.serialize())")
        // a sibling is not under alice
        let e = await call(f.k, ["type": "get", "ref": "openai/main"], as: "eve", secret: MARK_GET)
        #expect(e.result["reason"] == "not_found")
    }

    @Test func forgetDropsTheValueAndTheGrants() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let out = await call(f.k, ["type": "forget", "ref": "openai/main"], as: "bob")
        #expect(out.result == ["forgotten": true, "ref": "openai/main"])
        #expect(record(f.k)["refs"]?["openai/main"] == .null)
        #expect(!(try String(contentsOf: f.file, encoding: .utf8)).contains(KEY))
        #expect(mode(f.file) == 0o600, "a rewrite keeps the file 0600")
    }

    @Test func aFileOthersCanReadIsRefusedLoud() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        chmod(f.file.path, 0o644)
        let out = await call(f.k, ["type": "get", "ref": "openai/main"], secret: MARK_GET)
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

    /// The file's bytes are python's `yaml.safe_dump(..., sort_keys=True,
    /// default_flow_style=False)` — either runtime reads the other's file.
    @Test func theFileIsPythonsBytesKeysSorted() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        for (ref, fields) in [
            ("z/ref", ["token": JSON.string("tok-1"), "api_key": .string(KEY)]),
            ("openai/main", ["api_key": JSON.string("sk-x")]),
        ] as [(String, OrderedDictionary<String, JSON>)] {
            let out = await call(
                f.k, ["type": "set", "ref": .string(ref), "host": "h.example", "fields": .object(fields)],
                secret: MARK_SET)
            #expect(out.status == .completed, "\(out.asJSON.serialize())")
        }
        #expect(
            try String(contentsOf: f.file, encoding: .utf8)
                == "refs:\n  openai/main:\n    api_key: sk-x\n  z/ref:\n    api_key: \(KEY)\n"
                + "    token: tok-1\n")
        // the reply names the fields sorted
        let back = await call(f.k, ["type": "get", "ref": "z/ref"], secret: MARK_GET)
        #expect(back.result["fields"].asObject.map { Array($0.keys) } == ["api_key", "token"])
        // forgetting both leaves python's empty map
        _ = await call(f.k, ["type": "forget", "ref": "z/ref"])
        _ = await call(f.k, ["type": "forget", "ref": "openai/main"])
        #expect(try String(contentsOf: f.file, encoding: .utf8) == "refs: {}\n")
    }

    /// A RELATIVE `file` lives under the kernel's running dir, clamped — `..`
    /// cannot climb out of it.
    @Test func aRelativeFileIsClampedUnderTheRunningDir() async throws {
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
        _ = await create(k, [
            "parent_id": "kernel_state", "handler_module": "credential_store.tools", "id": "creds",
            "file": "sub/creds.yaml",
        ])
        let out = await call(
            k, ["type": "set", "ref": "r", "host": "h", "fields": ["api_key": .string(KEY)]],
            secret: MARK_SET)
        #expect(out.status == .completed, "\(out.asJSON.serialize())")
        #expect(mode(dir.appendingPathComponent("sub/creds.yaml")) == 0o600)
        _ = await create(k, [
            "parent_id": "kernel_state", "handler_module": "credential_store.tools", "id": "out",
            "file": "../escape.yaml",
        ])
        let esc = await call(
            k, ["type": "set", "ref": "r", "host": "h", "fields": ["api_key": .string(KEY)]],
            secret: MARK_SET, target: "out")
        #expect(esc.status == .failed)
        #expect(esc.result["error"] == "ValueError: fs: root '..' escapes the running dir")
        // the reflect names the file as written
        let refl = await call(k, ["type": "reflect"])
        #expect(refl.result["file"] == "sub/creds.yaml" && refl.result["backend"] == "file")
    }

    // ─── the keychain backend (an in-memory double — never the real one) ────

    @Test func theKeychainBackendKeepsOneItemPerRef() async throws {
        let ring = MemKeychain()
        let f = await mk(keychain: ring, ["backend": "keychain"])
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        #expect(ring.items == ["fantastic\u{0}creds/openai/main": #"{"api_key": "\#(KEY)"}"#])
        #expect(!FileManager.default.fileExists(atPath: f.file.path), "no file is written")
        let got = await call(f.k, ["type": "get", "ref": "openai/main"], secret: MARK_GET)
        #expect(got.result["fields"] == ["api_key": .string(KEY)])
        _ = await call(f.k, ["type": "forget", "ref": "openai/main"])
        #expect(ring.items.isEmpty)
        // reflect: no file for a keychain store
        let refl = await call(f.k, ["type": "reflect"])
        #expect(refl.result.asObject?["file"] == nil && refl.result["backend"] == "keychain")
    }

    @Test func theKeychainServiceIsTheRecords() async throws {
        let ring = MemKeychain()
        let f = await mk(keychain: ring, ["backend": "keychain", "keychain_service": "acme"])
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
                    "readers": ["alice"], "host": "api.example", "env": ["api_key": "PATH"],
                ],
                "ci/gone": [
                    "readers": ["alice"], "host": "api.example", "env": ["api_key": .string(unset)],
                ],
            ],
        ])
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let got = await call(f.k, ["type": "get", "ref": "ci/key"], as: "alice", secret: MARK_GET)
        #expect(
            got.result
                == .object([
                    "ref": "ci/key", "host": "api.example", "fields": ["api_key": .string(path)],
                ]), "\(got.result.serialize())")
        let w = await call(
            f.k,
            [
                "type": "set", "ref": "ci/key", "host": "api.example",
                "fields": ["api_key": .string(KEY2)],
            ], secret: MARK_SET)
        #expect(w.result["arg"] == "backend", "\(w.result.serialize())")
        let gone = await call(f.k, ["type": "get", "ref": "ci/gone"], as: "alice", secret: MARK_GET)
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
            (["keychain_service": 5], "keychain_service must be a non-empty string"),
            (["creators": ["", "x"]], "creators must be a list of agent ids"),
            (["refs": ["a", "b"]], "refs must be a map of ref to {readers, writers, host, fields, env}"),
            (["refs": [" ": [:]]], "a ref must be a non-empty string"),
            (["refs": ["r": "x"]], "refs['r'] must be a map"),
            (["refs": ["r": ["readers": "alice"]]], "refs['r'].readers must be a list of strings"),
            (["refs": ["r": ["host": "  "]]], "refs['r'].host must be a non-empty host"),
            (["refs": ["r": ["env": ["api_key": ""]]]], "refs['r'].env must map field names to variable names"),
        ]
        for (meta, text) in cases {
            #expect(b.validateMeta(meta, record: [:]) == text, "\(meta)")
        }
        #expect(b.validateMeta(["backend": .null, "refs": ["r": ["host": "https://h/x"]]], record: [:]) == nil)
    }

    /// The store stamps `holds_secrets: true` on its own record at boot (ai_core
    /// refuses every model-parsed call to an agent so marked), shows it on
    /// reflect, and refuses any other value at the write — create and update
    /// alike.
    ///
    /// Two checks judge the value, in a FIXED order: the kernel's shape check
    /// first (anything but `true`/`null` is malformed on every agent — its text),
    /// then the store's `validateMeta` (it refuses `null`, the operator's clear,
    /// on itself — the store's text). So `null` gets `holds` and every other
    /// value the kernel's.
    @Test func aStoreIsASecretHolderAndCannotStopBeingOne() async throws {
        let holds = "holds_secrets must be true — a credential_store always holds secrets"
        func shape(_ got: String) -> String {
            "holds_secrets must be exactly true (this agent holds secrets) or null "
                + "(the operator clears the mark) — got \(got)"
        }
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        #expect(record(f.k)["holds_secrets"] == .bool(true))
        let shown = await call(f.k, ["type": "reflect"])
        #expect(shown.result["holds_secrets"] == .bool(true), "\(shown.result.serialize())")
        let cases: [(JSON, String)] = [
            (.null, holds), (false, shape("bool")), ("true", shape("str")), (1, shape("int")),
        ]
        for (bad, why) in cases {
            let r = await f.k.send([
                Item("kernel", .object(["type": "update_agent", "id": "creds", "holds_secrets": bad]))
            ])[0].result
            #expect(
                r == .object(["error": .string(why), "reason": "invalid_meta", "id": "creds"]),
                "\(bad.serialize()): \(r.serialize())")
        }
        #expect(record(f.k)["holds_secrets"] == .bool(true))
        let ok = await f.k.send([
            Item("kernel", .object(["type": "update_agent", "id": "creds", "holds_secrets": true]))
        ])[0].result
        #expect(ok["updated"] == .bool(true), "\(ok.serialize())")
        for (bad, why) in [(JSON.null, holds), (JSON.bool(false), shape("bool"))] {
            let r = await create(
                f.k,
                [
                    "parent_id": "kernel_state",
                    "handler_module": .string(CredentialStoreBundle.handlerModule),
                    "id": "creds2", "holds_secrets": bad,
                ])
            #expect(
                r == .object(["error": .string(why), "reason": "invalid_meta"]), "\(r.serialize())")
            #expect(f.k.agent(AgentId("creds2")) == nil)
        }
    }

    // ─── the verbs' own refusals ────────────────────────────────────────────

    @Test func theArgumentRefusalsArePythons() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        let noRef = await call(f.k, ["type": "set", "host": "h", "fields": ["k": "v"]], secret: nil)
        #expect(
            noRef.result
                == .object([
                    "error": "credential_store set: ref required", "reason": "missing_arg",
                    "arg": "ref",
                ]))
        let noHost = await call(f.k, ["type": "set", "ref": "r", "host": "://x", "fields": ["k": "v"]])
        #expect(noHost.result["error"] == "credential_store set: host required")
        let badFields = await call(f.k, ["type": "set", "ref": "r", "host": "h", "fields": ["k": ""]])
        #expect(
            badFields.result["error"]
                == "credential_store set: fields must be a non-empty map of field name to string value")
        #expect(badFields.result["arg"] == "fields")
        // each field marked one by one is a mark too
        let perField = await call(
            f.k, ["type": "set", "ref": "r", "host": "h", "fields": ["a": "1", "b": "2"]],
            secret: ["payload.fields.a", "payload.fields.b"])
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
            f.k, ["type": "get", "ref": "r"], secret: ["result.fields"])
        #expect(fieldMark.result["fields"] == ["a": "1", "b": "2"], "\(fieldMark.result.serialize())")
    }

    @Test func aReadPinnedToAnotherHostIsRefused() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let evil = await call(
            f.k, ["type": "get", "ref": "openai/main", "host": "https://evil.example.net/v1"],
            secret: MARK_GET)
        #expect(
            evil.result
                == .object([
                    "error": .string(
                        "credential_store: ref 'openai/main' is bound to host 'api.openai.com', not "
                            + "'evil.example.net' — refused, nothing read"),
                    "reason": "bad_arg", "arg": "host",
                ]), "\(evil.result.serialize())")
        let ok = await call(
            f.k, ["type": "get", "ref": "openai/main", "host": "https://API.openai.com/v1/responses"],
            secret: MARK_GET)
        #expect(ok.result["fields"] == ["api_key": .string(KEY)], "\(ok.result.serialize())")
    }

    @Test func theReflectIsPostureNeverAValue() async throws {
        let f = await mk()
        defer { try? FileManager.default.removeItem(at: f.dir) }
        await seed(f.k)
        let r = await call(f.k, ["type": "reflect"])
        #expect(r.result["backend"] == "file" && r.result["refs"] == 1)
        #expect(r.result["file"] == .string(f.file.path))
        #expect(
            r.result["verbs"].asObject.map { Array($0.keys) }
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
