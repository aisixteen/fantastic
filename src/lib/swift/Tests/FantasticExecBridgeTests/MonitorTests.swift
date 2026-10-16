// exec_bridge MONITORS — a filter on a running job's output that
// SENDS what matches to an agent as a turn; polls nothing; throttled by `every`;
// capped by `tail`; a kernel task that `cancel`/`unmonitor` ends and that ends with
// its job. Mirrors python's `test_monitor.py` and rust's `tests_monitor.rs`.

#if os(macOS)

    import FantasticJSON
    @testable import FantasticKernel
    import Foundation
    import OrderedCollections
    import Testing

    @testable import FantasticExecBridge

    // MARK: - harness

    /// Records every message it is sent.
    private final class Ear: AgentBundle, @unchecked Sendable {
        let name = "monitor_ear"
        private let lock = NSLock()
        private var log: [(Date, String)] = []

        func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
            if payload["type"].asString == "send" {
                lock.withLock { log.append((Date(), payload["text"].asString ?? "")) }
            }
            return .object(["ok": .bool(true)])
        }

        var texts: [String] { lock.withLock { log.map(\.1) } }
        var stamps: [Date] { lock.withLock { log.map(\.0) } }
    }

    /// Prints `n` lines, one every `gap` seconds; FATAL on the ones listed; then sleeps.
    private let PRINTER = """
        import sys, time
        n, gap, fatal, hold = int(sys.argv[1]), float(sys.argv[2]), set(sys.argv[3].split(',')), float(sys.argv[4])
        for k in range(n):
            print(('FATAL step %d' if str(k) in fatal else 'INFO step %d') % k, flush=True)
            time.sleep(gap)
        time.sleep(hold)
        """

    private func py3() -> String? {
        for p in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"]
        where FileManager.default.isExecutableFile(atPath: p) { return p }
        return nil
    }

    private func call(_ k: Kernel, _ target: String, _ payload: OrderedDictionary<String, JSON>) async
        -> JSON
    {
        await k.send([Item(AgentId(target), .object(payload))])[0].result
    }

    /// A kernel with a `runner` (exec_bridge, open, any command) that may `send` to an
    /// `ear` that admits it.
    private func setup() async -> (Kernel, Ear) {
        let ear = Ear()
        let registry = BundleRegistry()
        registry.register("exec_bridge.tools", ExecBridgeBundle())
        registry.registerUnlisted("monitor_ear.tools", ear)
        let k = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        var r = await call(
            k, "kernel_state",
            [
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "id": .string("ear"), "handler_module": .string("monitor_ear.tools"),
                "callable_by": .object(["runner": .string("*")]),
            ])
        #expect(r["id"].asString == "ear", "\(r)")
        r = await call(
            k, "kernel_state",
            [
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "id": .string("runner"), "handler_module": .string("exec_bridge.tools"),
                "ingress_rule": .string("allow_all_ingress"), "cwd": .string("."),
                "inherit_env": .array([.string("PATH")]), "allowed": .array([.string(".*")]),
                "can_call": .object(["ear": .array([.string("send")])]),
            ])
        #expect(r["id"].asString == "runner", "\(r)")
        return (k, ear)
    }

    private func printer(
        _ k: Kernel, _ py: String, n: Int, gap: Double, fatal: String = "", hold: Double = 5
    ) async -> Int64 {
        let r = await call(
            k, "runner",
            [
                "type": .string("start"), "command": .string(py),
                "args": .array(
                    ["-c", PRINTER, "\(n)", "\(gap)", fatal.isEmpty ? "-" : fatal, "\(hold)"].map(
                        JSON.string)),
            ])
        guard case .integer(let jid) = r["job_id"] else {
            Issue.record("start: \(r)")
            return -1
        }
        return jid
    }

    private func monitor(_ k: Kernel, _ args: OrderedDictionary<String, JSON>) async -> JSON {
        var p: OrderedDictionary<String, JSON> = ["type": .string("monitor")]
        for (key, v) in args { p[key] = v }
        return await call(k, "runner", p)
    }

    private func until(_ what: String, secs: Double = 6, _ pred: () async -> Bool) async -> Bool {
        let deadline = Date().addingTimeInterval(secs)
        while Date() < deadline {
            if await pred() { return true }
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        Issue.record("\(what) never held within \(secs)s")
        return false
    }

    private func liveMonitors(_ k: Kernel) async -> [JSON] {
        await call(k, "runner", ["type": .string("monitors")])["monitors"].asArray ?? []
    }

    /// Every job the test left running is stopped.
    private func stopAll(_ k: Kernel) async {
        _ = await call(k, "runner", ["type": .string("stop")])
        try? await Task.sleep(nanoseconds: 200_000_000)
    }

    // MARK: - the five cases

    @Suite("exec_bridge monitors")
    struct MonitorTests {

        @Test("a match is sent with its header and nothing else")
        func matchSentWithHeader() async throws {
            guard let py = py3() else { return }
            let (k, ear) = await setup()
            let jid = await printer(k, py, n: 6, gap: 0.05, fatal: "2,4")
            let r = await monitor(
                k,
                [
                    "job_id": .integer(jid), "notify": .string("ear"), "match": .string("FATAL"),
                    "label": .string("w fatal"),
                ])
            guard case .integer(let mid) = r["monitor_id"] else {
                Issue.record("monitor: \(r)")
                return
            }
            #expect(
                r
                    == .object([
                        "monitor_id": .integer(mid), "job_id": .integer(jid),
                        "notify": .string("ear"), "match": .string("FATAL"), "every": .null,
                        "tail": .integer(100), "label": .string("w fatal"),
                    ]))
            _ = await until("two FATALs") {
                ear.texts.map { $0.components(separatedBy: "FATAL").count - 1 }.reduce(0, +) >= 2
            }
            let texts = ear.texts
            let joined = texts.joined(separator: "\n")
            #expect(joined.contains("FATAL step 2") && joined.contains("FATAL step 4"), "\(texts)")
            #expect(!joined.contains("INFO"), "only matching lines are sent")
            #expect(
                texts.first?.hasPrefix("[monitor \(mid) · job \(jid) · from the operator's call ")
                    == true, "\(texts)")
            #expect(
                texts.first?.components(separatedBy: "\n").first?.contains("\"w fatal\"") == true)
            await stopAll(k)
        }

        @Test("every throttles and tail caps")
        func everyThrottlesTailCaps() async throws {
            guard let py = py3() else { return }
            let (k, ear) = await setup()
            let jid = await printer(k, py, n: 40, gap: 0.025)  // ~1 s of output, 40 lines
            let r = await monitor(
                k,
                [
                    "job_id": .integer(jid), "notify": .string("ear"), "every": .double(0.4),
                    "tail": .integer(5),
                ])
            #expect(r["monitor_id"].asInt != nil, "\(r)")
            // waited ON the deliveries, not on a clock: a loaded runner starts the
            // printer late, and a fixed sleep then sees too little of its output
            _ = await until("two deliveries, one cut by tail", secs: 8) {
                ear.stamps.count >= 2 && ear.texts.contains { $0.contains("earlier lines dropped") }
            }
            let stamps = ear.stamps
            #expect((2...5).contains(stamps.count), "\(stamps.count) deliveries for ~1 s at every=0.4")
            let gaps = zip(stamps, stamps.dropFirst()).map { $1.timeIntervalSince($0) }
            #expect(gaps.allSatisfy { $0 >= 0.35 }, "\(gaps)")
            for text in ear.texts {
                let lines = text.components(separatedBy: "\n").dropFirst()
                    .filter { $0.hasPrefix("INFO") || $0.hasPrefix("FATAL") }
                #expect(lines.count <= 5, "\(text)")
            }
            #expect(ear.texts.contains { $0.contains("earlier lines dropped") }, "tail cut something")
            await stopAll(k)
        }

        @Test("without match every is required and args are checked")
        func argsChecked() async throws {
            guard let py = py3() else { return }
            let (k, _) = await setup()
            let jid = await printer(k, py, n: 1, gap: 0.01, hold: 3)
            let cases: [(OrderedDictionary<String, JSON>, String, String)] = [
                (["notify": .string("ear")], "missing_arg", "every"),
                (["notify": .string("ear"), "match": .string("("), "every": .integer(1)], "bad_arg", "match"),
                (["notify": .string("ear"), "match": .string("x"), "every": .integer(0)], "bad_arg", "every"),
                (["notify": .string("ear"), "match": .string("x"), "tail": .integer(0)], "bad_arg", "tail"),
                (["match": .string("x")], "missing_arg", "notify"),
                (["notify": .string("nobody"), "match": .string("x")], "not_found", "notify"),
            ]
            for (extra, reason, arg) in cases {
                var a: OrderedDictionary<String, JSON> = ["job_id": .integer(jid)]
                for (key, v) in extra { a[key] = v }
                let r = await monitor(k, a)
                #expect(r["reason"].asString == reason && r["arg"].asString == arg, "\(extra) → \(r)")
            }
            let r = await monitor(
                k, ["job_id": .integer(999_999), "notify": .string("ear"), "match": .string("x")])
            #expect(r["reason"].asString == "not_found", "\(r)")
            await stopAll(k)
        }

        @Test("cancel and unmonitor end a monitor, and the job keeps running")
        func cancelAndUnmonitor() async throws {
            guard let py = py3() else { return }
            let (k, ear) = await setup()
            let jid = await printer(k, py, n: 400, gap: 0.02, hold: 2)  // ~8 s of lines
            let a =
                await monitor(
                    k,
                    [
                        "job_id": .integer(jid), "notify": .string("ear"), "match": .string("INFO"),
                        "label": .string("a"),
                    ])["monitor_id"].asInt ?? -1
            let b =
                await monitor(
                    k,
                    [
                        "job_id": .integer(jid), "notify": .string("ear"), "match": .string("INFO"),
                        "label": .string("b"),
                    ])["monitor_id"].asInt ?? -1
            let listed = await liveMonitors(k)
            #expect(Set(listed.compactMap { $0["monitor_id"].asInt }) == [a, b], "\(listed)")
            let ps = await call(k, "kernel", ["type": .string("ps")])["tasks"].asArray ?? []
            let monitorRows = Set(
                ps.filter { $0["verb"].asString == "monitor" }.compactMap {
                    $0["id"].asInt
                })
            #expect(monitorRows.isSuperset(of: [a, b]), "\(ps)")
            _ = await until("both monitors delivered") {
                ear.texts.contains { $0.contains("\"a\"") } && ear.texts.contains { $0.contains("\"b\"") }
            }
            // kernel cancel ends `a`; `unmonitor` ends `b`
            let out = await call(k, "kernel", ["type": .string("cancel"), "task_id": .integer(a)])
            #expect(out["cancelled"].asBool == true, "\(out)")
            let un = await call(k, "runner", ["type": .string("unmonitor"), "monitor_id": .integer(b)])
            #expect(un == .object(["unmonitored": .bool(true), "monitor_id": .integer(b)]), "\(un)")
            _ = await until("both monitors gone") { await liveMonitors(k).isEmpty }
            try await Task.sleep(nanoseconds: 300_000_000)  // an in-flight delivery may still land
            let before = ear.texts.count
            try await Task.sleep(nanoseconds: 800_000_000)
            #expect(ear.texts.count == before, "nothing more is sent after the cancel")
            let st = await call(k, "runner", ["type": .string("status"), "job_id": .integer(jid)])
            #expect(
                st["jobs"].asArray?.first?["status"].asString == "running",
                "cancelling a monitor never touches its job")
            _ = await call(k, "runner", ["type": .string("stop"), "job_id": .integer(jid)])
            let gone = await call(k, "runner", ["type": .string("unmonitor"), "monitor_id": .integer(a)])
            #expect(gone["reason"].asString == "not_found", "\(gone)")
            await stopAll(k)
        }

        @Test("a monitor ends with its job, delivering what waited")
        func endsWithJob() async throws {
            guard let py = py3() else { return }
            let (k, ear) = await setup()
            let jid = await printer(k, py, n: 3, gap: 0.01, fatal: "1", hold: 0.3)
            let r = await monitor(
                k, ["job_id": .integer(jid), "notify": .string("ear"), "every": .integer(30)])
            #expect(r["monitor_id"].asInt != nil, "\(r)")
            // every=30: the first line goes at once, the rest wait — and go when the job ends
            _ = await until("the final delivery at job end", secs: 5) {
                ear.texts.contains { $0.contains("job ended") }
            }
            #expect(ear.texts.joined(separator: "\n").contains("FATAL step 1"))
            _ = await until("monitor gone") { await liveMonitors(k).isEmpty }
        }

        @Test("a refused delivery ends the monitor, and monitors says why")
        func refusedDeliveryEndsMonitor() async throws {
            guard let py = py3() else { return }
            let (k, ear) = await setup()
            let inbox = k.ensureInbox(AgentId("runner"))
            let u = await call(
                k, "kernel_state",
                [
                    "type": .string("update_agent"), "id": .string("ear"),
                    "callable_by": .object(["$self": .string("*")]),
                ])
            #expect(u["updated"].asBool == true, "\(u)")
            var jid: Int64 = -1
            var mid: Int64 = -1
            var dead: [JSON] = []
            let logged = await RuntimeLog.capture {
                jid = await printer(k, py, n: 30, gap: 0.05)
                mid =
                    await monitor(
                        k, ["job_id": .integer(jid), "notify": .string("ear"), "match": .string("INFO")]
                    )["monitor_id"].asInt ?? -1
                _ = await until("the monitor is dead") {
                    await liveMonitors(k).first?["status"].asString == "dead"
                }
                // the event lands right after the status flips: wait for it, then drain the rest
                _ = await until("the monitor_dead event") {
                    while let it = inbox.tryTake() {
                        if it.payload["type"].asString == "monitor_dead" { dead.append(it.payload) }
                    }
                    return !dead.isEmpty
                }
                try? await Task.sleep(nanoseconds: 100_000_000)
                while let it = inbox.tryTake() {
                    if it.payload["type"].asString == "monitor_dead" { dead.append(it.payload) }
                }
            }
            let rows = await liveMonitors(k)
            #expect(rows.count == 1, "\(rows)")
            #expect(rows.first?["monitor_id"].asInt == mid, "\(rows)")
            #expect(
                rows.first?["error"].asString?.contains("ear does not accept 'send' from runner") == true,
                "\(rows)")
            #expect(rows.first?["delivered"].asInt == 0, "\(rows)")
            #expect(ear.texts.isEmpty)
            // loud: the event on the bridge's inbox ...
            let want: JSON = .object([
                "type": .string("monitor_dead"), "monitor_id": .integer(mid), "job_id": .integer(jid),
                "notify": .string("ear"), "reason": .string("unauthorized"),
                "error": rows.first?["error"] ?? .null,
            ])
            #expect(dead == [want], "\(dead)")
            // ... and the warning line
            let head = "exec_bridge runner: monitor \(mid) (job \(jid)) dead — ear refused the delivery: "
            #expect(
                logged.contains {
                    $0.hasPrefix(head) && $0.contains("ear does not accept 'send' from runner")
                }, "\(logged)")
            // dead stays dead: later output is not sent
            try await Task.sleep(nanoseconds: 300_000_000)
            let again = await liveMonitors(k)
            #expect(again.first?["status"].asString == "dead", "\(again)")
            #expect(again.first?["delivered"].asInt == 0, "\(again)")
            let un = await call(k, "runner", ["type": .string("unmonitor"), "monitor_id": .integer(mid)])
            #expect(un == .object(["unmonitored": .bool(true), "monitor_id": .integer(mid)]), "\(un)")
            #expect(await liveMonitors(k).isEmpty)
            await stopAll(k)
        }
    }

#endif  // os(macOS)
