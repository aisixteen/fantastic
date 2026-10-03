// SECRET MARKS ON THE GUEST WIRE, end to end (owner-approved, 2026-10-02).
//
// A program spawned by exec_bridge talks to the kernel over FANTASTIC_CTRL_FD with
// `{op:"send", rid, target, payload, secret?}` — the connector's
// `kernel.send(target, payload=None, secret=None)` — and the relay copies `secret`
// onto the dispatched item. So a program marks a credential read exactly as any
// caller does: every record of the call shows `<secret>`, and the program, the
// receiver, holds the real value.
//
// Twin of python `exec_bridge/tests/test_exec_bridge.py::
// test_a_program_marks_a_credential_store_set_and_get`.

#if os(macOS)

    import FantasticCredentialStore
    import FantasticExecBridge
    import FantasticJSON
    import FantasticKernel
    import Foundation
    import OrderedCollections
    import Testing

    private func py3() -> String? {
        for p in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"]
        where FileManager.default.isExecutableFile(atPath: p) { return p }
        return nil
    }

    /// Every state event a kernel publishes.
    private final class Tap: @unchecked Sendable {
        private let lock = NSLock()
        private var _all: [JSON] = []
        var all: [JSON] { lock.withLock { _all } }
        func add(_ e: JSON) { lock.withLock { _all.append(e) } }
    }

    /// The program: mint a key INSIDE the process, store it marked, read it back
    /// marked, and try once unmarked.
    private let CREDS_CHILD = """
        import json, secrets
        key = 'sk-guest-' + secrets.token_hex(16)
        s = kernel.send('creds', {'type': 'set', 'ref': 'guest', 'fields': {'api_key': key}}, secret=['payload.fields'])
        g = kernel.send('creds', {'type': 'get', 'ref': 'guest'}, secret=['result'])
        u = kernel.send('creds', {'type': 'get', 'ref': 'guest'})
        print(json.dumps({'stored': s.get('stored'), 'match': (g.get('fields') or {}).get('api_key') == key, 'unmarked': [u.get('reason'), u.get('arg')]}), flush=True)

        """

    @Suite("exec_bridge guest wire — a program holds a credential")
    struct ExecGuestSecretsTests {

        /// A spawned program stores and reads a credential over the guest wire with
        /// the same marks any caller uses: it gets the real value back, every record
        /// of the calls shows `<secret>`, and an unmarked `get` is refused. The key
        /// is minted INSIDE the program, so the only copies are the store's file and
        /// the program.
        @Test(.timeLimit(.minutes(1)))
        func aProgramMarksACredentialStoreSetAndGet() async throws {
            guard let py = py3() else { print("skipping: no python3"); return }
            let dir = FileManager.default.temporaryDirectory
                .appendingPathComponent("guest-creds-\(UUID().uuidString.prefix(8))")
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            defer { try? FileManager.default.removeItem(at: dir) }
            let r = BundleRegistry()
            r.register(
                CredentialStoreBundle.handlerModule, CredentialStoreBundle(keychain: MemKeychain()))
            r.register("exec_bridge.tools", ExecBridgeBundle())
            let k = Kernel(storage: .inMemory, bundles: r)
            let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
            k.register(root)
            k.setRoot(root)
            let file = dir.appendingPathComponent("creds.yaml")
            for p: JSON in [
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": .string(CredentialStoreBundle.handlerModule), "id": "creds",
                    "file": .string(file.path),
                    "refs": [
                        "guest": [
                            "host": "api.example.com", "readers": ["runner"], "writers": ["runner"],
                        ]
                    ],
                    "callable_by": ["runner": "*"],
                ],
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "exec_bridge.tools", "id": "runner",
                    "ingress_rule": "allow_all_ingress", "cwd": .string(dir.path),
                    "allowed": [.string("\(NSRegularExpression.escapedPattern(for: py)) -c .*")],
                    "can_call": ["creds": "*"],
                ],
            ] {
                let made = await k.send([Item(AgentId("kernel"), p)])[0].result
                #expect(made["error"].isNull, "\(made.serialize())")
            }
            let tap = Tap()
            k.subscribe { ev, _ in tap.add(ev) }

            // the reference wire client, as a program gets it: the `connector` verb
            let connector = await k.send([
                Item(AgentId("runner"), .object(["type": "connector"]))
            ])[0].result["source"].asString ?? ""
            #expect(connector.contains("def send(self, target, payload=None, secret=None)"))
            let started = await k.send([
                Item(
                    AgentId("runner"),
                    .object([
                        "type": "start", "command": .string(py),
                        "args": ["-c", .string(connector + CREDS_CHILD)],
                    ]))
            ])[0].result
            let job = try #require(started["job_id"].asInt, "\(started.serialize())")
            var row: JSON = .null
            let deadline = Date().addingTimeInterval(15)
            while Date() < deadline {
                let st = await k.send([
                    Item(AgentId("runner"), .object(["type": "status", "job_id": .integer(job)]))
                ])[0].result
                row = st["jobs"].asArray?.first ?? .null
                if !row["exit_code"].isNull { break }
                try await Task.sleep(nanoseconds: 50_000_000)
            }
            #expect(row["exit_code"] == 0, "\(row.serialize())")
            let last = try JSON.parseStrict(row["last_line"].asString ?? "")
            #expect(
                last == ["stored": true, "match": true, "unmarked": ["bad_arg", "secret"]],
                "\(row.serialize())")
            let stored = (try? String(contentsOf: file, encoding: .utf8)) ?? ""
            let key = try #require(
                stored.range(of: "sk-guest-[0-9a-f]{32}", options: .regularExpression).map {
                    String(stored[$0])
                }, "the store's file holds the key: \(stored)")
            let calls = Dictionary(
                k.rows.all().filter {
                    $0["kind"]?.asString == "call" && $0["agent"]?.asString == "creds"
                        && $0["secret"] != nil
                }.map { ($0["verb"]?.asString ?? "", $0) }, uniquingKeysWith: { a, _ in a })
            #expect(calls["set"]?["secret"] == ["payload.fields"], "\(calls)")
            #expect(calls["get"]?["secret"] == ["result"], "\(calls)")
            #expect(calls["set"]?["payload"]?["fields"] == .string(Secret.placeholder), "\(calls)")
            let ledger = await k.send([Item(AgentId("kernel"), .object(["type": "ledger"]))])[0]
            for (place, what) in [
                ("rows", JSON.array(k.rows.all().map { JSON.object($0) })),
                ("ledger", ledger.result),
                ("state stream", JSON.array(tap.all)),
                ("records", JSON.array(k.allAgents().map { JSON.object($0.meta) })),
                ("job row", row),
            ] {
                #expect(!what.serialize().contains(key), "the key reached the \(place)")
            }
        }
    }

#endif  // os(macOS)
