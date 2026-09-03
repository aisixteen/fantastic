// 8G: terminal_backend PTY smoke tests (macOS only).

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import FantasticKernelStartup
    import FantasticTerminalBackend
    import Foundation
    import Testing

    @Suite("terminal_backend PTY")
    struct TerminalBackendTests {
        @Test func bootsShellAndEmitsOutput() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            // Use /bin/echo via a one-shot bash so the test deterministically terminates.
            _ = await kernel.send([Item(AgentId("core"), .object([
                    "type": .string("create_agent"),
                    "parent_id": .string("core"),
                    "handler_module": .string("terminal_backend.tools"),
                    "id": .string("term"),
                    "shell": .string("/bin/bash"),
                ]))])[0].result

            // Subscribe to the agent's inbox so we can read output emit events.
            let inbox = kernel.ensureInbox(AgentId("ws_watcher"))
            kernel.watch(src: AgentId("term"), watcher: AgentId("ws_watcher"))

            let bootReply = await kernel.send([Item(AgentId("term"), .object(["type": .string("boot")]))])[0].result
            #expect(bootReply["ok"].asBool == true, "boot failed: \(bootReply.serialize())")
            let pid = bootReply["pid"].asInt ?? 0
            #expect(pid > 0, "expected non-zero pid; got reply: \(bootReply.serialize())")

            // Write a command — echo + newline.
            _ = await kernel.send([Item(AgentId("term"), .object([
                    "type": .string("input"),
                    "data": .string("echo hello-pty\n"),
                ]))])[0].result

            // Wait for output containing "hello-pty".
            var got: String = ""
            let deadline = Date().addingTimeInterval(3)
            var iter = inbox.makeAsyncIterator()
            while Date() < deadline {
                guard let event = await iter.next() else { break }
                if let chunk = event["chunk"].asString {
                    got += chunk
                    if got.contains("hello-pty") { break }
                }
            }
            #expect(got.contains("hello-pty"), "expected echo output; got \(got.prefix(200))")

            _ = await kernel.send([Item(AgentId("term"), .object(["type": .string("shutdown")]))])[0].result
        }

        /// Boot a `/bin/cat` session under `id`, watching its inbox for output.
        private func bootCat(_ kernel: Kernel, id: String) async throws -> (pid: Int32, inbox: AsyncStream<JSON>) {
            _ = await kernel.send([Item(AgentId("core"), .object([
                    "type": .string("create_agent"),
                    "parent_id": .string("core"),
                    "handler_module": .string("terminal_backend.tools"),
                    "id": .string(id),
                    "shell": .string("/bin/cat"),
                ]))])[0].result
            let inbox = kernel.ensureInbox(AgentId("\(id)_watcher"))
            kernel.watch(src: AgentId(id), watcher: AgentId("\(id)_watcher"))
            let boot = await kernel.send([Item(AgentId(id), .object(["type": .string("boot")]))])[0].result
            #expect(boot["ok"].asBool == true, "boot failed: \(boot.serialize())")
            let pid = Int32(boot["pid"].asInt ?? 0)
            #expect(pid > 0)
            return (pid, inbox)
        }

        /// Collect output chunks until `needle` appears (bounded).
        private func awaitOutput(_ inbox: AsyncStream<JSON>, containing needle: String, seconds: Double = 3) async -> String {
            var got = ""
            let deadline = Date().addingTimeInterval(seconds)
            var iter = inbox.makeAsyncIterator()
            while Date() < deadline {
                guard let event = await iter.next() else { break }
                if let chunk = event["chunk"].asString {
                    got += chunk
                    if got.contains(needle) { break }
                }
            }
            return got
        }

        /// `shutdown` must reap the child (no zombie: `kill(pid, 0)` → ESRCH,
        /// not success), and a second `shutdown` — plus the session's later
        /// `deinit` — must be a no-op, never a re-kill of a recycled pid or a
        /// double close of the fd number.
        @Test func shutdownIsIdempotentAndReapsTheChild() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (pid, _) = try await bootCat(kernel, id: "term_reap")
            #expect(kill(pid, 0) == 0, "child should be alive before shutdown")
            let first = await kernel.send([Item(AgentId("term_reap"), .object(["type": .string("shutdown")]))])[0].result
            #expect(first["ok"].asBool == true)
            let second = await kernel.send([Item(AgentId("term_reap"), .object(["type": .string("shutdown")]))])[0].result
            #expect(second["ok"].asBool == true, "second shutdown must be a no-op: \(second.serialize())")
            let deadline = Date().addingTimeInterval(2)
            var gone = false
            while Date() < deadline {
                if kill(pid, 0) == -1 && errno == ESRCH {
                    gone = true
                    break
                }
                try await Task.sleep(nanoseconds: 20_000_000)
            }
            #expect(gone, "pid \(pid) still exists (zombie or alive) 2 s after shutdown")
            let reflect = await kernel.send([Item(AgentId("term_reap"), .object(["type": .string("reflect")]))])[0].result
            #expect(reflect["running"].asBool == false)
        }

        /// `paste_image` takes the image as the BLOB of a binary frame (python
        /// parity, no base64): the file on disk holds exactly the bytes sent
        /// and the PTY receives `path + " "` (echoed back by the line discipline).
        @Test func pasteImageTakesRawBytesOnTheBinaryChannel() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            let (_, inbox) = try await bootCat(kernel, id: "term_paste")
            // A PNG signature followed by bytes that are NOT valid UTF-8 —
            // anything that survived a text detour would be mangled.
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
            _ = await kernel.send([Item(AgentId("term_paste"), .object(["type": .string("shutdown")]))])[0].result
            #expect(!FileManager.default.fileExists(atPath: path), "scratch file must go with the session")
        }

        /// The text channel has NO base64 field — `data_base64` is refused as a
        /// missing blob, naming the binary channel.
        @Test func pasteImageRejectsBase64Field() async throws {
            let kernel = try await startKernelInMemory(portHint: 0)
            _ = try await bootCat(kernel, id: "term_b64")
            let reply = await kernel.send([Item(AgentId("term_b64"), .object([
                    "type": .string("paste_image"),
                    "data_base64": .string("iVBORw0KGgo="),
                ]))])[0].result
            #expect(reply["reason"].asString == "missing_arg", "reply: \(reply.serialize())")
            #expect(reply["arg"].asString == "blob")
            #expect(reply["ok"].asBool == nil)
            _ = await kernel.send([Item(AgentId("term_b64"), .object(["type": .string("shutdown")]))])[0].result
        }
    }

#endif
