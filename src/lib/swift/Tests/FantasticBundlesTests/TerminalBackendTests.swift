// terminal_backend — the PTY session, python's verbs and reply shapes (macOS only).

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import FantasticKernelStartup
    import FantasticTerminalBackend
    import Foundation
    import Testing

    @Suite("terminal_backend PTY")
    struct TerminalBackendTests {
        /// Boot a `command` session under `id`, watching its inbox for output.
        private func boot(_ kernel: Kernel, id: String, command: String, args: [String] = [])
            async throws
            -> (pid: Int32, inbox: Inbox)
        {
            _ = await kernel.send([
                Item(
                    AgentId("kernel_state"),
                    .object([
                        "type": .string("create_agent"),
                        "parent_id": .string("kernel_state"),
                        "handler_module": .string("terminal_backend.tools"),
                        "id": .string(id),
                        "command": .string(command),
                        "args": .array(args.map { .string($0) }),
                    ]))
            ])[0].result
            // `create_agent` boots the child through the kernel's fanout, so the
            // session is already streaming before a watcher could exist. Drop
            // that one and start the session under the watch.
            _ = await kernel.send([Item(AgentId(id), .object(["type": .string("stop")]))])[0].result
            let inbox = kernel.ensureInbox(AgentId("\(id)_watcher"))
            kernel.watch(src: AgentId(id), watcher: AgentId("\(id)_watcher"))
            let booted = await kernel.send([Item(AgentId(id), .object(["type": .string("boot")]))])[
                0
            ].result
            // Python's `_boot` answers `{running:true}` and nothing else.
            #expect(booted == .object(["running": .bool(true)]), "boot: \(booted.serialize())")
            let r = await kernel.send([Item(AgentId(id), .object(["type": .string("reflect")]))])[0]
                .result
            let pid = Int32(r["pid"].asInt ?? 0)
            #expect(pid > 0, "reflect carries no pid: \(r.serialize())")
            #expect(r["running"].asBool == true && r["command"].asString == command)
            return (pid, inbox)
        }

        /// Collect `output` chunks (python's field is `data`) until `needle` appears.
        private func awaitOutput(
            _ inbox: Inbox, containing needle: String, seconds: Double = 3
        ) async -> String {
            var got = ""
            let deadline = Date().addingTimeInterval(seconds)
            var iter = inbox.makeAsyncIterator()
            while Date() < deadline {
                guard let event = await iter.next()?.payload else { break }
                if event["type"].asString == "output", let chunk = event["data"].asString {
                    got += chunk
                    if got.contains(needle) { break }
                }
            }
            return got
        }

        @Test func bootsShellWriteEmitsOutputAndScrollback() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (_, inbox) = try await boot(
                kernel, id: "term", command: "/bin/bash", args: ["--norc", "--noprofile"])
            let w = await kernel.send([
                Item(
                    AgentId("term"),
                    .object([
                        "type": .string("write"),
                        "data": .string("echo hello-pty\n"),
                    ]))
            ])[0].result
            #expect(w["written"].asInt == 15, "\(w.serialize())")
            let got = await awaitOutput(inbox, containing: "hello-pty")
            #expect(got.contains("hello-pty"), "expected echo output; got \(got.prefix(200))")
            // `output` is the scrollback tail; `max_bytes` slices it.
            let out = await kernel.send([
                Item(AgentId("term"), .object(["type": .string("output")]))
            ])[0].result
            #expect(out["output"].asString?.contains("hello-pty") == true)
            let tail = await kernel.send([
                Item(
                    AgentId("term"), .object(["type": .string("output"), "max_bytes": .integer(4)]))
            ])[0].result
            #expect(tail["output"].asString?.unicodeScalars.count == 4)
            let r = await kernel.send([Item(AgentId("term"), .object(["type": .string("reflect")]))]
            )[0].result
            #expect((r["scrollback_bytes"].asInt ?? 0) > 0 && (r["unacked"].asInt ?? 0) > 0)
            #expect(r["paused"].asBool == false)
            _ = await kernel.send([Item(AgentId("term"), .object(["type": .string("stop")]))])[0]
                .result
        }

        @Test func shellRunsOneCommandAndReturnsItsOutput() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(
                kernel, id: "term_sh", command: "/bin/bash", args: ["--norc", "--noprofile"])
            let r = await kernel.send([
                Item(
                    AgentId("term_sh"),
                    .object([
                        "type": .string("shell"), "cmd": .string("echo one; echo two"),
                        "timeout": .integer(10),
                    ]))
            ])[0].result
            #expect(r["completed"].asBool == true, "\(r.serialize())")
            #expect(r["cmd"].asString == "echo one; echo two")
            let out = r["output"].asString ?? ""
            // Python's slice: the echoed command line (token and all) plus the
            // command's lines, cut before printf's own newline + token.
            #expect(out.contains("one") && out.contains("two"), "\(out)")
            #expect(!out.hasSuffix("__"), "printf's token line must be cut: \(out)")
            // A command that never finishes times out, gets ^C, and says so.
            let t = await kernel.send([
                Item(
                    AgentId("term_sh"),
                    .object([
                        "type": .string("shell"), "cmd": .string("sleep 30"),
                        "timeout": .double(0.3),
                    ]))
            ])[0].result
            #expect(
                t["completed"].asBool == false && t["error"].asString == "timeout",
                "\(t.serialize())")
            let bad = await kernel.send([
                Item(AgentId("term_sh"), .object(["type": .string("shell")]))
            ])[0].result
            #expect(bad["error"].asString == "shell: cmd required")
            _ = await kernel.send([Item(AgentId("term_sh"), .object(["type": .string("stop")]))])[0]
                .result
            let dead = await kernel.send([
                Item(
                    AgentId("term_sh"), .object(["type": .string("shell"), "cmd": .string("true")]))
            ])[0].result
            #expect(dead["error"].asString == "shell: not running; call boot first")
        }

        @Test func ackResizeSignalRestartArePythonsShapes() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (pid, _) = try await boot(kernel, id: "term_v", command: "/bin/cat")
            let a = await kernel.send([
                Item(AgentId("term_v"), .object(["type": .string("ack"), "chars": .integer(5)]))
            ])[0].result
            #expect(a["unacked"].asInt == 0 && a["paused"].asBool == false, "\(a.serialize())")
            let rz = await kernel.send([
                Item(
                    AgentId("term_v"),
                    .object([
                        "type": .string("resize"), "cols": .integer(120), "rows": .integer(40),
                    ]))
            ])[0].result
            #expect(
                rz
                    == .object([
                        "resized": .bool(true), "cols": .integer(120), "rows": .integer(40),
                    ]), "\(rz.serialize())")
            let r = await kernel.send([
                Item(AgentId("term_v"), .object(["type": .string("reflect")]))
            ])[0].result
            #expect(r["cols"].asInt == 120 && r["rows"].asInt == 40)
            // `signal` defaults to SIGINT=2 and answers {signal}.
            let sg = await kernel.send([
                Item(
                    AgentId("term_v"), .object(["type": .string("signal"), "signal": .integer(0)]))
            ])[0].result
            #expect(sg == .object(["signal": .integer(0)]), "\(sg.serialize())")
            // `restart` respawns: a new pid, the old one reaped.
            let rs = await kernel.send([
                Item(AgentId("term_v"), .object(["type": .string("restart")]))
            ])[0].result
            #expect(rs == .object(["restarted": .bool(true)]))
            let r2 = await kernel.send([
                Item(AgentId("term_v"), .object(["type": .string("reflect")]))
            ])[0].result
            let pid2 = Int32(r2["pid"].asInt ?? 0)
            #expect(pid2 > 0 && pid2 != pid, "\(r2.serialize())")
            let unknown = await kernel.send([
                Item(AgentId("term_v"), .object(["type": .string("zap")]))
            ])[0].result
            #expect(unknown["error"].asString == "terminal: unknown type 'zap'")
            _ = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("stop")]))])[0]
                .result
            let nr = await kernel.send([
                Item(AgentId("term_v"), .object(["type": .string("write"), "data": .string("x")]))
            ])[0].result
            #expect(nr == .object(["error": .string("not running")]))
        }

        /// The child's exit is EOF on the master: python emits `closed` and cleans up.
        @Test func childExitEmitsClosedAndClearsRunning() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (_, inbox) = try await boot(kernel, id: "term_exit", command: "/bin/cat")
            _ = await kernel.send([
                Item(
                    AgentId("term_exit"),
                    .object(["type": .string("write"), "data": .string("\u{04}")]))
            ])[0].result
            var closed = false
            let deadline = Date().addingTimeInterval(3)
            var iter = inbox.makeAsyncIterator()
            while Date() < deadline, let ev = await iter.next()?.payload {
                if ev["type"].asString == "closed" {
                    closed = true
                    break
                }
            }
            #expect(closed, "no `closed` event after the child exited")
            try await Task.sleep(nanoseconds: 100_000_000)
            let r = await kernel.send([
                Item(AgentId("term_exit"), .object(["type": .string("reflect")]))
            ])[0].result
            #expect(r["running"].asBool == false && r["pid"].isNull)
        }

        /// `stop` must reap the child (no zombie: `kill(pid, 0)` → ESRCH), and a
        /// second `stop` — plus the session's later `deinit` — must be a no-op.
        @Test func stopIsIdempotentAndReapsTheChild() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (pid, _) = try await boot(kernel, id: "term_reap", command: "/bin/cat")
            #expect(kill(pid, 0) == 0, "child should be alive before stop")
            let first = await kernel.send([
                Item(AgentId("term_reap"), .object(["type": .string("stop")]))
            ])[0].result
            #expect(first["stopped"].asBool == true, "\(first.serialize())")
            let second = await kernel.send([
                Item(AgentId("term_reap"), .object(["type": .string("stop")]))
            ])[0].result
            #expect(
                second["stopped"].asBool == true,
                "second stop must be a no-op: \(second.serialize())")
            let deadline = Date().addingTimeInterval(2)
            var gone = false
            while Date() < deadline {
                if kill(pid, 0) == -1 && errno == ESRCH {
                    gone = true
                    break
                }
                try await Task.sleep(nanoseconds: 20_000_000)
            }
            #expect(gone, "pid \(pid) still exists (zombie or alive) 2 s after stop")
            let reflect = await kernel.send([
                Item(AgentId("term_reap"), .object(["type": .string("reflect")]))
            ])[0].result
            #expect(reflect["running"].asBool == false)
        }

        /// `paste_image` takes the image as the BLOB of a binary frame (python
        /// parity, no base64): the file on disk holds exactly the bytes sent
        /// and the PTY receives `path + " "` (echoed back by the line discipline).
        @Test func pasteImageTakesRawBytesOnTheBinaryChannel() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (_, inbox) = try await boot(kernel, id: "term_paste", command: "/bin/cat")
            var blob = Data([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
            blob.append(contentsOf: (0..<4096).map { UInt8(($0 * 7 + 0x80) & 0xFF) })
            let (reply, body) = await kernel.sendWithBinary(
                AgentId("term_paste"),
                .object(["type": .string("paste_image"), "mime": .string("image/png")]),
                blob)
            #expect(body == nil, "paste_image returns no chunk")
            #expect(reply["bytes"].asInt == Int64(blob.count), "reply: \(reply.serialize())")
            let path = try #require(
                reply["path"].asString, "reply carries no path: \(reply.serialize())")
            #expect(path.hasSuffix(".png"))
            #expect(path.contains("fantastic_paste_term_paste_"))
            let onDisk = try Data(contentsOf: URL(fileURLWithPath: path))
            #expect(onDisk == blob, "file bytes differ from the blob")
            let got = await awaitOutput(inbox, containing: path + " ")
            #expect(
                got.contains(path + " "), "PTY did not receive path + space; got \(got.prefix(300))"
            )
            let bad = await kernel.sendWithBinary(
                AgentId("term_paste"),
                .object(["type": .string("paste_image"), "mime": .string("image/bmp")]), blob)
            #expect(bad.0["error"].asString == "paste_image: unsupported image type 'image/bmp'")
            _ = await kernel.send([Item(AgentId("term_paste"), .object(["type": .string("stop")]))]
            )[0].result
            #expect(
                !FileManager.default.fileExists(atPath: path),
                "scratch file must go with the session")
        }

        /// The text channel carries no bytes — python's refusal, byte for byte.
        @Test func pasteImageOnTheTextChannelIsRefusedLikePython() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(kernel, id: "term_b64", command: "/bin/cat")
            let reply = await kernel.send([
                Item(
                    AgentId("term_b64"),
                    .object([
                        "type": .string("paste_image"),
                        "data_base64": .string("iVBORw0KGgo="),
                    ]))
            ])[0].result
            #expect(
                reply == .object(["error": .string("paste_image: data must be bytes")]),
                "reply: \(reply.serialize())")
            _ = await kernel.send([Item(AgentId("term_b64"), .object(["type": .string("stop")]))])[
                0
            ].result
        }

        // ── python-parity twins (A6) ────────────────────────────────

        /// A6.2 — the bundle readme is python's `readme.md`, byte for byte.
        @Test func readmeIsPythonVerbatim() throws {
            let expected = try String(
                contentsOf: pythonSource("terminal/src/terminal_backend/readme.md"),
                encoding: .utf8)
            #expect(TerminalBackendBundle().readme == expected)
        }

        /// A6.1 — an exec failure happens in the CHILD: `boot` still answers
        /// `{running:true}`, the PTY carries python's `f"{cmd}: {exc}\r\n"` line
        /// and the child exits 127, so `closed` follows.
        @Test func execFailureIsWrittenByTheChildAndThenCloses() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = await kernel.send([
                Item(
                    AgentId("kernel_state"),
                    .object([
                        "type": .string("create_agent"),
                        "parent_id": .string("kernel_state"),
                        "handler_module": .string("terminal_backend.tools"),
                        "id": .string("term_noexec"),
                        "command": .string("/no/such/shell"),
                    ]))
            ])[0].result
            _ = await kernel.send([Item(AgentId("term_noexec"), .object(["type": .string("stop")]))]
            )[0].result
            let inbox = kernel.ensureInbox(AgentId("term_noexec_watcher"))
            kernel.watch(src: AgentId("term_noexec"), watcher: AgentId("term_noexec_watcher"))
            let booted = await kernel.send([
                Item(AgentId("term_noexec"), .object(["type": .string("boot")]))
            ])[0].result
            #expect(booted == .object(["running": .bool(true)]), "\(booted.serialize())")
            var text = ""
            var closed = false
            let deadline = Date().addingTimeInterval(4)
            var iter = inbox.makeAsyncIterator()
            while Date() < deadline, let ev = await iter.next()?.payload {
                if ev["type"].asString == "output" { text += ev["data"].asString ?? "" }
                if ev["type"].asString == "closed" {
                    closed = true
                    break
                }
            }
            // `str(FileNotFoundError)` names the path python passed to execve.
            #expect(
                text.contains(
                    "/no/such/shell: [Errno 2] No such file or directory: '/no/such/shell'"),
                "PTY line was: \(text.debugDescription)")
            #expect(closed, "no `closed` after the child's _exit(127)")
        }

        /// A6.3 — every non-empty READ emits, even the one that decodes to `''`:
        /// the first two bytes of a 3-byte char arrive alone and python still
        /// emits `{type:'output', data:''}`.
        @Test func aSplitMultiByteCharEmitsAnEmptyDataEvent() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (_, inbox) = try await boot(
                kernel, id: "term_split", command: "/bin/sh",
                args: ["-c", "printf '\\342\\224'; sleep 0.4; printf '\\200'; sleep 1"])
            var sawEmpty = false
            var joined = ""
            let deadline = Date().addingTimeInterval(4)
            var iter = inbox.makeAsyncIterator()
            while Date() < deadline, let ev = await iter.next()?.payload {
                guard ev["type"].asString == "output" else { continue }
                let chunk = ev["data"].asString ?? "<none>"
                if chunk.isEmpty { sawEmpty = true }
                joined += chunk
                if joined.contains("\u{2500}") { break }
            }
            #expect(
                sawEmpty, "no empty-data output event; chunks joined to \(joined.debugDescription)")
            #expect(
                joined.contains("\u{2500}"),
                "the split char never reassembled: \(joined.debugDescription)")
            _ = await kernel.send([Item(AgentId("term_split"), .object(["type": .string("stop")]))]
            )[0].result
        }

        /// A6.4 — `data.encode(...)` on a non-str is python's AttributeError, so
        /// the DISPATCH fails; it is not an `{error}` reply.
        @Test func writeWithNonStringDataFailsTheDispatch() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(kernel, id: "term_enc", command: "/bin/cat")
            let r = await kernel.send([
                Item(
                    AgentId("term_enc"),
                    .object([
                        "type": .string("write"), "data": .integer(1),
                    ]))
            ])[0].result
            #expect(
                r
                    == .object([
                        "error": .string("AttributeError: 'int' object has no attribute 'encode'"),
                        "raised": .bool(true),
                    ]), "\(r.serialize())")
            _ = await kernel.send([Item(AgentId("term_enc"), .object(["type": .string("stop")]))])[
                0
            ].result
        }

        /// A6.5 — `struct.pack("HHHH", …)` range-checks every winsize: 70000
        /// columns raises (no trap, no wrap) and the kernel answers the NEXT
        /// call normally. Python's `struct.error.__name__` is `error`.
        @Test func anOutOfUshortResizeRaisesLikeStructPack() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(kernel, id: "term_ws", command: "/bin/cat")
            let r = await kernel.send([
                Item(
                    AgentId("term_ws"),
                    .object([
                        "type": .string("resize"), "cols": .integer(70000), "rows": .integer(40),
                    ]))
            ])[0].result
            #expect(
                r
                    == .object([
                        "error": .string("error: ushort format requires 0 <= number <= 65535"),
                        "raised": .bool(true),
                    ]), "\(r.serialize())")
            // Python stores cols/rows BEFORE it packs, so the raise leaves them set.
            let reflect = await kernel.send([
                Item(AgentId("term_ws"), .object(["type": .string("reflect")]))
            ])[0].result
            #expect(
                reflect["cols"].asInt == 70000 && reflect["rows"].asInt == 40,
                "\(reflect.serialize())")
            let neg = await kernel.send([
                Item(
                    AgentId("term_ws"),
                    .object([
                        "type": .string("resize"), "cols": .integer(-1), "rows": .integer(40),
                    ]))
            ])[0].result
            #expect(neg["error"].asString == "error: argument out of range", "\(neg.serialize())")
            _ = await kernel.send([Item(AgentId("term_ws"), .object(["type": .string("stop")]))])[0]
                .result
        }

        /// A6.24 — `shutdown` is not one of python's eleven verbs.
        @Test func shutdownIsAnUnknownType() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(kernel, id: "term_sd", command: "/bin/cat")
            let r = await kernel.send([
                Item(AgentId("term_sd"), .object(["type": .string("shutdown")]))
            ])[0].result
            #expect(
                r == .object(["error": .string("terminal: unknown type 'shutdown'")]),
                "\(r.serialize())")
            _ = await kernel.send([Item(AgentId("term_sd"), .object(["type": .string("stop")]))])[0]
                .result
        }

        /// A6.25 — the reader attachment is registered work: `ps` carries a
        /// `pty_reader` row for the terminal agent (python `_attach_reader`).
        @Test func theReaderAttachmentIsAPsRow() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(kernel, id: "term_ps", command: "/bin/cat")
            let ps = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[
                0
            ].result
            let rows = ps["tasks"].asArray ?? []
            #expect(
                rows.contains {
                    $0["agent"].asString == "term_ps" && $0["verb"].asString == "pty_reader"
                }, "no pty_reader row: \(ps.serialize())")
            // C36 — the attachment is a `task` ROW too, under the same id, so
            // the reader survives in the message log and not only in `ps`.
            let stored = kernel.rows.all().filter { $0["agent"]?.asString == "term_ps" }
            #expect(
                stored.contains {
                    $0["kind"]?.asString == "task" && $0["verb"]?.asString == "pty_reader"
                }, "no pty_reader task row: \(stored.map { JSON.object($0).serialize() })")
            _ = await kernel.send([Item(AgentId("term_ps"), .object(["type": .string("stop")]))])[0]
                .result
        }

        /// A6.27 — `output max_bytes` follows python's `text[-n:]`: 0 keeps the
        /// whole scrollback (`text[0:]`), a negative n drops that many leading
        /// code points. Neither traps.
        @Test func outputMaxBytesSlicesLikePython() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(
                kernel, id: "term_slice", command: "/bin/bash", args: ["--norc", "--noprofile"])
            _ = await kernel.send([
                Item(
                    AgentId("term_slice"),
                    .object([
                        "type": .string("write"), "data": .string("echo abcdefgh\n"),
                    ]))
            ])[0].result
            var whole = ""
            let deadline = Date().addingTimeInterval(3)
            while Date() < deadline {
                whole =
                    await kernel.send([
                        Item(AgentId("term_slice"), .object(["type": .string("output")]))
                    ])[0].result["output"].asString ?? ""
                if whole.contains("abcdefgh") { break }
                try await Task.sleep(nanoseconds: 50_000_000)
            }
            #expect(whole.contains("abcdefgh"), "\(whole.debugDescription)")
            // WAIT FOR THE BUFFER TO SETTLE before comparing three reads of it:
            // the login shell's own banner can still be arriving, and a read
            // that landed between `whole` and `zero` made them differ for a
            // reason that has nothing to do with `max_bytes` (measured under
            // the full parallel run, 2026-09-20). Two equal reads in a row = no
            // more output in flight.
            let settleBy = Date().addingTimeInterval(3)
            while Date() < settleBy {
                try await Task.sleep(nanoseconds: 100_000_000)
                let again =
                    await kernel.send([
                        Item(AgentId("term_slice"), .object(["type": .string("output")]))
                    ])[0].result["output"].asString ?? ""
                if again == whole { break }
                whole = again
            }
            let zero = await kernel.send([
                Item(
                    AgentId("term_slice"),
                    .object([
                        "type": .string("output"), "max_bytes": .integer(0),
                    ]))
            ])[0].result
            #expect(zero["output"].asString == whole, "max_bytes 0 must keep the whole text")
            let negative = await kernel.send([
                Item(
                    AgentId("term_slice"),
                    .object([
                        "type": .string("output"), "max_bytes": .integer(-3),
                    ]))
            ])[0].result
            let dropped = String(String.UnicodeScalarView(whole.unicodeScalars.dropFirst(3)))
            #expect(
                negative["output"].asString == dropped,
                "max_bytes -3 must drop three leading code points")
            _ = await kernel.send([Item(AgentId("term_slice"), .object(["type": .string("stop")]))]
            )[0].result
        }

        /// A6.28 — falsy record fields fall through to python's defaults
        /// (`rec.get(k) or <default>`), and a STOPPED reflect returns the raw
        /// record cols/rows.
        @Test func falsyRecordFieldsFallBackToPythonDefaults() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = await kernel.send([
                Item(
                    AgentId("kernel_state"),
                    .object([
                        "type": .string("create_agent"),
                        "parent_id": .string("kernel_state"),
                        "handler_module": .string("terminal_backend.tools"),
                        "id": .string("term_falsy"),
                        "command": .string(""),
                        "cols": .integer(0),
                        "rows": .integer(0),
                    ]))
            ])[0].result
            _ = await kernel.send([Item(AgentId("term_falsy"), .object(["type": .string("stop")]))]
            )[0].result
            // Stopped: the RECORD's values, raw — no `int()`, no truthiness.
            let cold = await kernel.send([
                Item(AgentId("term_falsy"), .object(["type": .string("reflect")]))
            ])[0].result
            #expect(
                cold["cols"] == .integer(0) && cold["rows"] == .integer(0), "\(cold.serialize())")
            #expect(cold["command"].asString == detectShellForTest(), "\(cold.serialize())")
            _ = await kernel.send([Item(AgentId("term_falsy"), .object(["type": .string("boot")]))]
            )[0].result
            let hot = await kernel.send([
                Item(AgentId("term_falsy"), .object(["type": .string("reflect")]))
            ])[0].result
            #expect(hot["cols"].asInt == 200 && hot["rows"].asInt == 50, "\(hot.serialize())")
            #expect(hot["command"].asString == detectShellForTest(), "\(hot.serialize())")
            _ = await kernel.send([Item(AgentId("term_falsy"), .object(["type": .string("stop")]))]
            )[0].result
        }

        /// A6.30 — the emits are ORDERED: one consumer drains the reader in read
        /// order, so a numbered burst arrives in sequence and `closed` is last.
        @Test func outputEventsArriveInReadOrderAndClosedIsLast() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (_, inbox) = try await boot(
                kernel, id: "term_order", command: "/bin/sh",
                args: [
                    "-c",
                    "i=0; while [ $i -lt 40 ]; do printf 'L%s\\n' $i; i=$((i+1)); sleep 0.01; done",
                ])
            // Concatenated IN ARRIVAL ORDER: a shuffled delivery shows up as
            // shuffled numbers, which is exactly what this pins.
            var text = ""
            var closed = false
            let deadline = Date().addingTimeInterval(8)
            var iter = inbox.makeAsyncIterator()
            while Date() < deadline, let ev = await iter.next()?.payload {
                if ev["type"].asString == "closed" {
                    closed = true
                    break
                }
                guard ev["type"].asString == "output" else { continue }
                text += ev["data"].asString ?? ""
            }
            let seen = text.replacingOccurrences(of: "\r\n", with: "\n")
                .split(separator: "\n")
                .compactMap { $0.hasPrefix("L") ? Int($0.dropFirst()) : nil }
            #expect(seen == Array(0..<40), "events arrived out of order: \(seen)")
            #expect(closed, "no `closed` after the burst")
        }

        /// A6.31 — an empty `mime` is python's `(x or "image/png").lower()`.
        @Test func pasteImageWithAnEmptyMimeSavesAPng() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(kernel, id: "term_mime", command: "/bin/cat")
            let (reply, _) = await kernel.sendWithBinary(
                AgentId("term_mime"),
                .object(["type": .string("paste_image"), "mime": .string("")]),
                Data([0x89, 0x50, 0x4E, 0x47]))
            let path = try #require(reply["path"].asString, "\(reply.serialize())")
            #expect(path.hasSuffix(".png"), "\(path)")
            _ = await kernel.send([Item(AgentId("term_mime"), .object(["type": .string("stop")]))])[
                0
            ].result
        }

        /// EIGHT PTYs FORKED AT ONCE, each one its own. Darwin's `forkpty` names
        /// the slave through `ptsname()`'s STATIC buffer, so two threads forking
        /// together could interleave there and one child would open the OTHER
        /// pair's slave: that master saw instant EOF, the session was cleaned up
        /// and `reflect` answered `running:false, pid:null` (measured: ~10 issues
        /// in one full `swift test` run in four). Python forks from a single
        /// thread; `PtySession.start` now serializes.
        @Test func concurrentSpawnsEachGetTheirOwnPty() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let ids = (0..<8).map { "term_par_\($0)" }
            for id in ids {
                _ = await kernel.send([
                    Item(
                        AgentId("kernel_state"),
                        .object([
                            "type": .string("create_agent"),
                            "parent_id": .string("kernel_state"),
                            "handler_module": .string("terminal_backend.tools"),
                            "id": .string(id),
                            "command": .string("/bin/cat"),
                        ]))
                ])[0].result
            }
            // `create_agent` boots inside its own dispatch, one at a time. Stop
            // them and re-boot in ONE batch: eight DIFFERENT targets dispatch
            // concurrently, so eight `forkpty` calls run at once.
            _ = await kernel.send(ids.map { Item(AgentId($0), .object(["type": .string("stop")])) })
            let booted = await kernel.send(
                ids.map { Item(AgentId($0), .object(["type": .string("boot")])) })
            for (id, o) in zip(ids, booted) {
                #expect(o.result == .object(["running": .bool(true)]), "\(id): \(o.result.serialize())")
            }
            let rs = await kernel.send(
                ids.map { Item(AgentId($0), .object(["type": .string("reflect")])) })
            for (id, o) in zip(ids, rs) {
                #expect(o.result["running"].asBool == true, "\(id): \(o.result.serialize())")
                #expect((o.result["pid"].asInt ?? 0) > 0, "\(id): \(o.result.serialize())")
            }
            _ = await kernel.send(ids.map { Item(AgentId($0), .object(["type": .string("stop")])) })
        }
    }

    /// Python `_detect_shell`, repeated in the test so the expectation is not
    /// read out of the code under test.
    private func detectShellForTest() -> String {
        let fm = FileManager.default
        if let sh = ProcessInfo.processInfo.environment["SHELL"], !sh.isEmpty,
            fm.fileExists(atPath: sh)
        {
            return sh
        }
        for cand in ["/bin/zsh", "/bin/bash", "/bin/sh"] where fm.fileExists(atPath: cand) {
            return cand
        }
        return "/bin/sh"
    }

#endif
