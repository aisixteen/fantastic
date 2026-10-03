// A job is DRIVEN by the caller that started it — the operator, always.
//
// Audit AV-8 (2026-10-03): every job verb checked only that the job belonged to
// THIS exec_bridge agent, so any caller granted `write` on a shared leg typed into
// another caller's shell (a shell job carries the agent's FANTASTIC_CTRL_FD
// identity) and killed its jobs. Now `start` records the job's `starter` (the
// calling agent, a foreign caller's `<guid>|<agent>`, or null for the operator)
// and the verbs that DRIVE a job — `write`, `resize`, `paste_image`, `stop`,
// `interrupt`, `clear` — refuse a named job another caller started; their bulk
// forms act on the caller's own jobs.
//
// The READ verbs — `status`, `output`, `ack` — stay gated by grants alone: the
// documented read-only viewer (granted `status`/`output`/`ack`, never `start`) is
// by construction NOT the starter, so scoping them to the starter would end it.
//
// Twin of python `exec_bridge/tests/test_job_starter.py`; the job is `/bin/sh`.
#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import OrderedCollections
    import Testing

    @testable import FantasticExecBridge

    /// Prints a line, then waits for one: a live PTY job until it is written to.
    private let CHILD = "echo READY; read x; echo GOT $x"

    private func term() async -> Kernel {
        let registry = BundleRegistry()
        registry.register("exec_bridge.tools", ExecBridgeBundle())
        let k = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        let made = await k.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "exec_bridge.tools", "id": "term",
                    "ingress_rule": "allow_all_ingress", "allowed": [".*"], "cwd": ".",
                    "env": ["PATH": "/usr/bin:/bin"], "callable_by": ["a": "*", "b": "*"],
                ]))
        ])[0].result
        #expect(made["id"] == "term", "\(made.serialize())")
        for who in ["a", "b"] {
            k.register(
                Agent(
                    id: AgentId(who), handlerModule: nil, parentId: AgentId("kernel_state"),
                    meta: ["can_call": ["term": "*"]]))
        }
        return k
    }

    private func call(
        _ k: Kernel, _ payload: OrderedDictionary<String, JSON>, as who: String? = nil,
        binary: Blob? = nil
    ) async -> JSON {
        let item = Item(AgentId("term"), .object(payload), binary: binary)
        guard let who else { return await k.send([item])[0].result }
        return await KernelTaskLocals.$currentSender.withValue(AgentId(who)) {
            await k.send([item])[0].result
        }
    }

    private func startAs(_ k: Kernel, _ who: String?) async -> Int64 {
        let r = await call(
            k,
            [
                "type": "start", "command": "/bin/sh", "args": ["-c", .string(CHILD)],
                "pty": ["cols": 80, "rows": 24],
            ], as: who)
        #expect(r["pty"] == true, "\(r.serialize())")
        return r["job_id"].asInt ?? -1
    }

    private func notYours(_ verb: String, _ jid: Int64) -> JSON {
        [
            "error": .string(
                "exec_bridge: job \(jid) is not yours — only the caller that started it (or the "
                    + "operator) may \(verb) it"),
            "reason": "unauthorized",
            "job_id": .integer(jid),
        ]
    }

    private func alive(_ k: Kernel, _ jid: Int64) async -> Bool {
        let s = await call(k, ["type": "status", "job_id": .integer(jid)])
        return s["jobs"][0]["status"] == "running"
    }

    private func done(_ k: Kernel) async {
        _ = await k.send([Item("kernel_state", ["type": "delete_agent", "id": "term"])])
    }

    @Suite("exec_bridge — a job is driven by its starter", .serialized)
    struct JobStarterTests {

        @Test(.timeLimit(.minutes(1)))
        func anotherCallerCannotDriveAJob() async {
            let k = await term()
            let jid = await startAs(k, "a")
            let cases: [(String, OrderedDictionary<String, JSON>)] = [
                ("write", ["data": "hijack\n"]),
                ("resize", ["cols": 100, "rows": 30]),
                ("stop", [:]),
                ("interrupt", [:]),
                ("clear", [:]),
            ]
            for (verb, extra) in cases {
                var p: OrderedDictionary<String, JSON> = ["type": .string(verb), "job_id": .integer(jid)]
                for (key, v) in extra { p[key] = v }
                let out = await call(k, p, as: "b")
                #expect(out == notYours(verb, jid), "\(verb) \(out.serialize())")
            }
            // the binary channel: the image rides at `data`
            let pasted = await call(
                k, ["type": "paste_image", "job_id": .integer(jid), "mime": "image/png"], as: "b",
                binary: Blob("data", Data([0x89, 0x50, 0x4E, 0x47])))
            #expect(pasted == notYours("paste_image", jid), "\(pasted.serialize())")
            #expect(await alive(k, jid), "b's verbs left a's job running")
            // the bulk forms act on b's own jobs — none
            #expect(await call(k, ["type": "stop"], as: "b") == ["killed": 0])
            #expect(await call(k, ["type": "interrupt"], as: "b") == ["interrupted": 0])
            #expect(await alive(k, jid))
            await done(k)
        }

        /// The viewer pattern the readme documents: granted only `status`/
        /// `output`/`ack`, it re-attaches to a terminal someone else started.
        @Test(.timeLimit(.minutes(1)))
        func aReadOnlyViewerReadsAJobItDidNotStart() async {
            let k = await term()
            let jid = await startAs(k, "a")
            let listed = await call(k, ["type": "status"], as: "b")
            #expect(
                (listed["jobs"].asArray ?? []).map { $0["job_id"] } == [.integer(jid)],
                "\(listed.serialize())")
            let one = await call(k, ["type": "status", "job_id": .integer(jid)], as: "b")
            #expect(one["jobs"][0]["job_id"] == .integer(jid), "\(one.serialize())")
            let ring = await call(k, ["type": "output", "job_id": .integer(jid)], as: "b")
            #expect(
                ring["job_id"] == .integer(jid) && ring["output"].asString != nil,
                "\(ring.serialize())")
            let acked = await call(
                k, ["type": "ack", "job_id": .integer(jid), "chars": 1], as: "b")
            #expect(
                Set(acked.asObject?.keys.map { $0 } ?? []) == ["unacked", "paused"],
                "\(acked.serialize())")
            await done(k)
        }

        @Test(.timeLimit(.minutes(1)))
        func theStarterAndTheOperatorDriveIt() async {
            let k = await term()
            let jid = await startAs(k, "a")
            #expect(
                await call(
                    k, ["type": "resize", "job_id": .integer(jid), "cols": 90, "rows": 30], as: "a")
                    == ["resized": true, "cols": 90, "rows": 30])
            let mine = await call(k, ["type": "status"], as: "a")
            #expect((mine["jobs"].asArray ?? []).map { $0["job_id"] } == [.integer(jid)])
            let everyone = await call(k, ["type": "status"])
            #expect((everyone["jobs"].asArray ?? []).map { $0["job_id"] } == [.integer(jid)])
            #expect(
                await call(k, ["type": "write", "job_id": .integer(jid), "data": "op\n"])
                    == ["written": 3])
            await done(k)
        }

        @Test(.timeLimit(.minutes(1)))
        func anOperatorStartedJobIsTheOperators() async {
            let k = await term()
            let jid = await startAs(k, nil)
            let out = await call(
                k, ["type": "write", "job_id": .integer(jid), "data": "x\n"], as: "a")
            #expect(out == notYours("write", jid), "\(out.serialize())")
            await done(k)
        }
    }

#endif
