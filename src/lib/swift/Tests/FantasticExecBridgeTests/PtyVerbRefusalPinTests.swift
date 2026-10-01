// exec_bridge's PTY verbs — every argument refusal, byte for byte.
//
// `PtyJobTests` drives the happy paths and checks refusal CODES. This file pins
// the whole reply each PTY verb answers on a bad argument (python
// `exec_bridge/tools.py` texts, key order included) and the order the checks run
// in: the job first (missing / not an int / not ours / not a PTY / ended), then
// the verb's own argument. A Result-to-typed-throws rewrite of these handlers
// must answer the same bytes. The jobs are `/bin/sh` — no interpreter needed.
#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import OrderedCollections
    import Testing

    @testable import FantasticExecBridge

    private func kernelWith(_ id: String) async -> Kernel {
        let registry = BundleRegistry()
        registry.register("exec_bridge.tools", ExecBridgeBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "exec_bridge.tools", "id": .string(id),
                    "ingress_rule": "allow_all_ingress", "allowed": [".*"], "cwd": ".",
                    "env": ["PATH": "/usr/bin:/bin"],
                ]))
        ])
        return kernel
    }

    private func call(_ k: Kernel, _ id: String, _ p: OrderedDictionary<String, JSON>) async
        -> String
    {
        await k.send([Item(AgentId(id), .object(p))])[0].result.serialize()
    }

    private func bad(_ verb: String, _ arg: String, _ want: String) -> String {
        #"{"error":"\#(verb): \#(arg) must be \#(want)","reason":"bad_arg","arg":"\#(arg)"}"#
    }

    @Suite("exec_bridge PTY verbs — refusal bodies", .serialized)
    struct PtyVerbRefusalPinTests {

        @Test func startRefusesABadPtyObjectColsFirst() async {
            let k = await kernelWith("pty_s")
            func start(_ pty: JSON) async -> String {
                await call(
                    k, "pty_s",
                    ["type": "start", "command": "/bin/sh", "args": ["-c", "exit 0"], "pty": pty])
            }
            #expect(await start("x") == bad("start", "pty", "an object {cols, rows}"))
            #expect(await start(.array([])) == bad("start", "pty", "an object {cols, rows}"))
            #expect(
                await start(["cols": 0, "rows": 24]) == bad("start", "cols", "a positive integer"))
            #expect(
                await start(["cols": true, "rows": 24])
                    == bad("start", "cols", "a positive integer"))
            #expect(await start(["cols": 80]) == bad("start", "rows", "a positive integer"))
            #expect(
                await start(["cols": 80, "rows": -1]) == bad("start", "rows", "a positive integer"))
            #expect(
                await start(["cols": "80", "rows": "x"])
                    == bad("start", "cols", "a positive integer"))
        }

        @Test func everyPtyVerbChecksTheJobFirst() async {
            let k = await kernelWith("pty_j")
            for verb in ["write", "resize", "ack", "paste_image", "output"] {
                #expect(
                    await call(k, "pty_j", ["type": .string(verb)])
                        == #"{"error":"\#(verb): job_id required","reason":"missing_arg","arg":"job_id"}"#
                )
                #expect(
                    await call(k, "pty_j", ["type": .string(verb), "job_id": "7"])
                        == bad(
                            verb, "job_id",
                            "an int — the job's task id (from `start`, `status` or `ps`)"))
                #expect(
                    await call(k, "pty_j", ["type": .string(verb), "job_id": 999999])
                        == #"{"error":"exec_bridge: no job 999999","reason":"not_found","job_id":999999}"#
                )
            }
        }

        /// On a PIPE job the PTY verbs refuse the job id, naming what they want.
        @Test func aPipeJobIsNotAPtyJob() async {
            let k = await kernelWith("pty_p")
            let started = await k.send([
                Item(
                    AgentId("pty_p"),
                    .object(["type": "start", "command": "/bin/sh", "args": ["-c", "read x"]]))
            ])[0].result
            let jid = started["job_id"]
            for verb in ["write", "resize", "ack", "paste_image"] {
                #expect(
                    await call(k, "pty_p", ["type": .string(verb), "job_id": jid])
                        == bad(verb, "job_id", "a PTY job (started with `pty`)"), "\(verb)")
            }
            #expect(
                await call(k, "pty_p", ["type": "output", "job_id": jid])
                    == bad("output", "job_id", "a PTY job (a pipe job's output is in `status`)"))
            _ = await call(k, "pty_p", ["type": "stop", "job_id": jid])
        }

        /// On a LIVE PTY job: each verb's own argument, then its success shape.
        @Test func aLivePtyJobsArgumentRefusalsAndReplies() async {
            let k = await kernelWith("pty_l")
            let started = await k.send([
                Item(
                    AgentId("pty_l"),
                    .object([
                        "type": "start", "command": "/bin/sh",
                        "args": ["-c", "read x; echo got $x"],
                        "pty": ["cols": 80, "rows": 24],
                    ]))
            ])[0].result
            #expect(started["pty"] == true, "\(started.serialize())")
            let jid = started["job_id"]
            let n = jid.asInt ?? -1

            #expect(
                await call(k, "pty_l", ["type": "write", "job_id": jid])
                    == bad("write", "data", "a string"))
            #expect(
                await call(k, "pty_l", ["type": "write", "job_id": jid, "data": 5])
                    == bad("write", "data", "a string"))

            #expect(
                await call(k, "pty_l", ["type": "resize", "job_id": jid])
                    == bad("resize", "cols", "a positive integer"))
            #expect(
                await call(k, "pty_l", ["type": "resize", "job_id": jid, "cols": 100, "rows": 0])
                    == bad("resize", "rows", "a positive integer"))
            #expect(
                await call(k, "pty_l", ["type": "resize", "job_id": jid, "cols": 100, "rows": 30])
                    == #"{"resized":true,"cols":100,"rows":30}"#)

            for chars: JSON in [.null, -1, "3", true, 1.5] {
                var p: OrderedDictionary<String, JSON> = ["type": "ack", "job_id": jid]
                if chars != .null { p["chars"] = chars }
                #expect(
                    await call(k, "pty_l", p) == bad("ack", "chars", "a non-negative integer"),
                    "\(chars)")
            }
            #expect(
                await call(k, "pty_l", ["type": "ack", "job_id": jid, "chars": 1_000_000])
                    == #"{"unacked":0,"paused":false}"#)

            for mb: JSON in [-1, "8", true] {
                #expect(
                    await call(k, "pty_l", ["type": "output", "job_id": jid, "max_bytes": mb])
                        == bad("output", "max_bytes", "a non-negative integer"), "\(mb)")
            }
            #expect(
                await call(k, "pty_l", ["type": "output", "job_id": jid, "max_bytes": 0])
                    == #"{"job_id":\#(n),"output":"","origin":"pty_l output {job_id: \#(n)} — the ring NOW; re-send, it moves"}"#
            )

            // paste_image: the text channel has no bytes; the binary one checks
            // size, then the mime (present, then known)
            #expect(
                await call(k, "pty_l", ["type": "paste_image", "job_id": jid, "mime": "image/png"])
                    == bad("paste_image", "data", "raw bytes"))
            func paste(_ header: JSON, _ data: Data) async -> String {
                await k.send([Item(AgentId("pty_l"), header, binary: data)])[0].result.serialize()
            }
            #expect(
                await paste(
                    .object(["type": "paste_image", "job_id": jid, "mime": "image/png"]),
                    Data(count: MAX_PASTE_IMAGE + 1))
                    == bad("paste_image", "data", "at most \(MAX_PASTE_IMAGE) bytes"))
            #expect(
                await paste(.object(["type": "paste_image", "job_id": jid, "mime": ""]), Data([1]))
                    == #"{"error":"paste_image: mime required","reason":"missing_arg","arg":"mime"}"#
            )
            #expect(
                await paste(
                    .object(["type": "paste_image", "job_id": jid, "mime": "image/bmp"]), Data([1]))
                    == bad(
                        "paste_image", "mime",
                        "one of ['image/gif', 'image/jpeg', 'image/png', 'image/webp']"))

            _ = await call(k, "pty_l", ["type": "write", "job_id": jid, "data": "bye\n"])
            _ = await call(k, "pty_l", ["type": "stop", "job_id": jid])
        }

        /// The events `inbox` holds within `seconds`, in order.
        private func events(_ inbox: Inbox, for seconds: Double) async -> [JSON] {
            var out: [JSON] = []
            let end = Date().addingTimeInterval(seconds)
            while Date() < end {
                if let item = inbox.tryTake() {
                    out.append(item.payload)
                } else {
                    try? await Task.sleep(nanoseconds: 20_000_000)
                }
            }
            return out
        }

        private func rowStatus(_ k: Kernel, _ id: String) async -> String {
            let st = await k.send([Item(AgentId(id), .object(["type": "status"]))])[0].result
            return st["jobs"].asArray?.first?["status"].asString ?? ""
        }

        /// A PTY job that exits non-zero ends `failed` and announces it last:
        /// `job_done {job_id, exit_code, ok:false}` — no stdout/stderr keys (the
        /// terminal's bytes went out as `output`).
        @Test func aPtyJobThatFailsAnnouncesJobDoneAndEndsFailed() async {
            let k = await kernelWith("pty_f")
            let inbox = k.ensureInbox(AgentId("pty_f"))
            let started = await k.send([
                Item(
                    AgentId("pty_f"),
                    .object([
                        "type": "start", "command": "/bin/sh", "args": ["-c", "exit 3"],
                        "pty": ["cols": 80, "rows": 24],
                    ]))
            ])[0].result
            let n = started["job_id"].asInt ?? -1
            var done: JSON? = nil
            let end = Date().addingTimeInterval(15)
            while done == nil && Date() < end {
                if let item = inbox.tryTake() {
                    if item.payload["type"] == "job_done" { done = item.payload }
                } else {
                    try? await Task.sleep(nanoseconds: 20_000_000)
                }
            }
            #expect(
                done?.serialize()
                    == #"{"type":"job_done","job_id":\#(n),"exit_code":3,"ok":false}"#,
                "\(done?.serialize() ?? "no job_done")")
            #expect(await rowStatus(k, "pty_f") == "failed")
        }

        /// A PTY job the KERNEL cancels ends `stopped` and announces NO
        /// `job_done` — a killed job never reads as one that finished.
        @Test func aCancelledPtyJobEndsStoppedWithNoJobDone() async throws {
            let k = await kernelWith("pty_c")
            let inbox = k.ensureInbox(AgentId("pty_c"))
            let started = await k.send([
                Item(
                    AgentId("pty_c"),
                    .object([
                        "type": "start", "command": "/bin/sh", "args": ["-c", "sleep 30"],
                        "pty": ["cols": 80, "rows": 24],
                    ]))
            ])[0].result
            let jid = try #require(started["job_id"].asInt)
            try await Task.sleep(nanoseconds: 200_000_000)
            _ = await k.send([
                Item(AgentId("kernel"), .object(["type": "cancel", "task_id": .integer(jid)]))
            ])
            var status = ""
            let end = Date().addingTimeInterval(10)
            while Date() < end {
                status = await rowStatus(k, "pty_c")
                // `stopping` is the signalled-not-yet-reaped step on the way
                if status != "running" && status != "stopping" { break }
                try await Task.sleep(nanoseconds: 50_000_000)
            }
            #expect(status == "stopped", "the cancelled PTY row settled as \(status)")
            let seen = await events(inbox, for: 1)
            #expect(!seen.contains { $0["type"] == "job_done" }, "\(seen.map { $0.serialize() })")
        }
    }
#endif
