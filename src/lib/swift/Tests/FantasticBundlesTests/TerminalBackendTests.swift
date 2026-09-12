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
        private func boot(_ kernel: Kernel, id: String, command: String, args: [String] = []) async throws
            -> (pid: Int32, inbox: AsyncStream<JSON>)
        {
            _ = await kernel.send([Item(AgentId("core"), .object([
                    "type": .string("create_agent"),
                    "parent_id": .string("core"),
                    "handler_module": .string("terminal_backend.tools"),
                    "id": .string(id),
                    "command": .string(command),
                    "args": .array(args.map { .string($0) }),
                ]))])[0].result
            let inbox = kernel.ensureInbox(AgentId("\(id)_watcher"))
            kernel.watch(src: AgentId(id), watcher: AgentId("\(id)_watcher"))
            let booted = await kernel.send([Item(AgentId(id), .object(["type": .string("boot")]))])[0].result
            // Python's `_boot` answers `{running:true}` and nothing else.
            #expect(booted == .object(["running": .bool(true)]), "boot: \(booted.serialize())")
            let r = await kernel.send([Item(AgentId(id), .object(["type": .string("reflect")]))])[0].result
            let pid = Int32(r["pid"].asInt ?? 0)
            #expect(pid > 0, "reflect carries no pid: \(r.serialize())")
            #expect(r["running"].asBool == true && r["command"].asString == command)
            return (pid, inbox)
        }

        /// Collect `output` chunks (python's field is `data`) until `needle` appears.
        private func awaitOutput(_ inbox: AsyncStream<JSON>, containing needle: String, seconds: Double = 3) async -> String {
            var got = ""
            let deadline = Date().addingTimeInterval(seconds)
            var iter = inbox.makeAsyncIterator()
            while Date() < deadline {
                guard let event = await iter.next() else { break }
                if event["type"].asString == "output", let chunk = event["data"].asString {
                    got += chunk
                    if got.contains(needle) { break }
                }
            }
            return got
        }

        @Test func bootsShellWriteEmitsOutputAndScrollback() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (_, inbox) = try await boot(kernel, id: "term", command: "/bin/bash", args: ["--norc", "--noprofile"])
            let w = await kernel.send([Item(AgentId("term"), .object([
                    "type": .string("write"),
                    "data": .string("echo hello-pty\n"),
                ]))])[0].result
            #expect(w["written"].asInt == 15, "\(w.serialize())")
            let got = await awaitOutput(inbox, containing: "hello-pty")
            #expect(got.contains("hello-pty"), "expected echo output; got \(got.prefix(200))")
            // `output` is the scrollback tail; `max_bytes` slices it.
            let out = await kernel.send([Item(AgentId("term"), .object(["type": .string("output")]))])[0].result
            #expect(out["output"].asString?.contains("hello-pty") == true)
            let tail = await kernel.send([Item(AgentId("term"), .object(["type": .string("output"), "max_bytes": .integer(4)]))])[0].result
            #expect(tail["output"].asString?.unicodeScalars.count == 4)
            let r = await kernel.send([Item(AgentId("term"), .object(["type": .string("reflect")]))])[0].result
            #expect((r["scrollback_bytes"].asInt ?? 0) > 0 && (r["unacked"].asInt ?? 0) > 0)
            #expect(r["paused"].asBool == false)
            _ = await kernel.send([Item(AgentId("term"), .object(["type": .string("stop")]))])[0].result
        }

        @Test func shellRunsOneCommandAndReturnsItsOutput() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(kernel, id: "term_sh", command: "/bin/bash", args: ["--norc", "--noprofile"])
            let r = await kernel.send([Item(AgentId("term_sh"), .object([
                    "type": .string("shell"), "cmd": .string("echo one; echo two"), "timeout": .integer(10),
                ]))])[0].result
            #expect(r["completed"].asBool == true, "\(r.serialize())")
            #expect(r["cmd"].asString == "echo one; echo two")
            let out = r["output"].asString ?? ""
            // Python's slice: the echoed command line (token and all) plus the
            // command's lines, cut before printf's own newline + token.
            #expect(out.contains("one") && out.contains("two"), "\(out)")
            #expect(!out.hasSuffix("__"), "printf's token line must be cut: \(out)")
            // A command that never finishes times out, gets ^C, and says so.
            let t = await kernel.send([Item(AgentId("term_sh"), .object([
                    "type": .string("shell"), "cmd": .string("sleep 30"), "timeout": .double(0.3),
                ]))])[0].result
            #expect(t["completed"].asBool == false && t["error"].asString == "timeout", "\(t.serialize())")
            let bad = await kernel.send([Item(AgentId("term_sh"), .object(["type": .string("shell")]))])[0].result
            #expect(bad["error"].asString == "shell: cmd required")
            _ = await kernel.send([Item(AgentId("term_sh"), .object(["type": .string("stop")]))])[0].result
            let dead = await kernel.send([Item(AgentId("term_sh"), .object(["type": .string("shell"), "cmd": .string("true")]))])[0].result
            #expect(dead["error"].asString == "shell: not running; call boot first")
        }

        @Test func ackResizeSignalRestartArePythonsShapes() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (pid, _) = try await boot(kernel, id: "term_v", command: "/bin/cat")
            let a = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("ack"), "chars": .integer(5)]))])[0].result
            #expect(a["unacked"].asInt == 0 && a["paused"].asBool == false, "\(a.serialize())")
            let rz = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("resize"), "cols": .integer(120), "rows": .integer(40)]))])[0].result
            #expect(rz == .object(["resized": .bool(true), "cols": .integer(120), "rows": .integer(40)]), "\(rz.serialize())")
            let r = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("reflect")]))])[0].result
            #expect(r["cols"].asInt == 120 && r["rows"].asInt == 40)
            // `signal` defaults to SIGINT=2 and answers {signal}.
            let sg = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("signal"), "signal": .integer(0)]))])[0].result
            #expect(sg == .object(["signal": .integer(0)]), "\(sg.serialize())")
            // `restart` respawns: a new pid, the old one reaped.
            let rs = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("restart")]))])[0].result
            #expect(rs == .object(["restarted": .bool(true)]))
            let r2 = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("reflect")]))])[0].result
            let pid2 = Int32(r2["pid"].asInt ?? 0)
            #expect(pid2 > 0 && pid2 != pid, "\(r2.serialize())")
            let unknown = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("zap")]))])[0].result
            #expect(unknown["error"].asString == "terminal: unknown type 'zap'")
            _ = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("stop")]))])[0].result
            let nr = await kernel.send([Item(AgentId("term_v"), .object(["type": .string("write"), "data": .string("x")]))])[0].result
            #expect(nr == .object(["error": .string("not running")]))
        }

        /// The child's exit is EOF on the master: python emits `closed` and cleans up.
        @Test func childExitEmitsClosedAndClearsRunning() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (_, inbox) = try await boot(kernel, id: "term_exit", command: "/bin/cat")
            _ = await kernel.send([Item(AgentId("term_exit"), .object(["type": .string("write"), "data": .string("\u{04}")]))])[0].result
            var closed = false
            let deadline = Date().addingTimeInterval(3)
            var iter = inbox.makeAsyncIterator()
            while Date() < deadline, let ev = await iter.next() {
                if ev["type"].asString == "closed" { closed = true; break }
            }
            #expect(closed, "no `closed` event after the child exited")
            try await Task.sleep(nanoseconds: 100_000_000)
            let r = await kernel.send([Item(AgentId("term_exit"), .object(["type": .string("reflect")]))])[0].result
            #expect(r["running"].asBool == false && r["pid"].isNull)
        }

        /// `stop` must reap the child (no zombie: `kill(pid, 0)` → ESRCH), and a
        /// second `stop` — plus the session's later `deinit` — must be a no-op.
        @Test func stopIsIdempotentAndReapsTheChild() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (pid, _) = try await boot(kernel, id: "term_reap", command: "/bin/cat")
            #expect(kill(pid, 0) == 0, "child should be alive before stop")
            let first = await kernel.send([Item(AgentId("term_reap"), .object(["type": .string("stop")]))])[0].result
            #expect(first["stopped"].asBool == true, "\(first.serialize())")
            let second = await kernel.send([Item(AgentId("term_reap"), .object(["type": .string("stop")]))])[0].result
            #expect(second["stopped"].asBool == true, "second stop must be a no-op: \(second.serialize())")
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
            let reflect = await kernel.send([Item(AgentId("term_reap"), .object(["type": .string("reflect")]))])[0].result
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
            #expect(body.isEmpty)
            #expect(reply["bytes"].asInt == Int64(blob.count), "reply: \(reply.serialize())")
            let path = try #require(reply["path"].asString, "reply carries no path: \(reply.serialize())")
            #expect(path.hasSuffix(".png"))
            #expect(path.contains("fantastic_paste_term_paste_"))
            let onDisk = try Data(contentsOf: URL(fileURLWithPath: path))
            #expect(onDisk == blob, "file bytes differ from the blob")
            let got = await awaitOutput(inbox, containing: path + " ")
            #expect(got.contains(path + " "), "PTY did not receive path + space; got \(got.prefix(300))")
            let bad = await kernel.sendWithBinary(
                AgentId("term_paste"), .object(["type": .string("paste_image"), "mime": .string("image/bmp")]), blob)
            #expect(bad.0["error"].asString == "paste_image: unsupported image type 'image/bmp'")
            _ = await kernel.send([Item(AgentId("term_paste"), .object(["type": .string("stop")]))])[0].result
            #expect(!FileManager.default.fileExists(atPath: path), "scratch file must go with the session")
        }

        /// The text channel carries no bytes — python's refusal, byte for byte.
        @Test func pasteImageOnTheTextChannelIsRefusedLikePython() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await boot(kernel, id: "term_b64", command: "/bin/cat")
            let reply = await kernel.send([Item(AgentId("term_b64"), .object([
                    "type": .string("paste_image"),
                    "data_base64": .string("iVBORw0KGgo="),
                ]))])[0].result
            #expect(reply == .object(["error": .string("paste_image: data must be bytes")]), "reply: \(reply.serialize())")
            _ = await kernel.send([Item(AgentId("term_b64"), .object(["type": .string("stop")]))])[0].result
        }
    }

#endif
