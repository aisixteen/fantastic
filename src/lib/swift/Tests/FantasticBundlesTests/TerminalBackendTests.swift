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
            _ = await kernel.send(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"),
                    "handler_module": .string("terminal_backend.tools"),
                    "id": .string("term"),
                    "shell": .string("/bin/bash"),
                ]))

            // Subscribe to the agent's inbox so we can read output emit events.
            let inbox = kernel.ensureInbox(AgentId("ws_watcher"))
            kernel.watch(src: AgentId("term"), watcher: AgentId("ws_watcher"))

            let bootReply = await kernel.send(
                AgentId("term"), .object(["type": .string("boot")]))
            #expect(bootReply["ok"].asBool == true, "boot failed: \(bootReply.serialize())")
            let pid = bootReply["pid"].asInt ?? 0
            #expect(pid > 0, "expected non-zero pid; got reply: \(bootReply.serialize())")

            // Write a command — echo + newline.
            _ = await kernel.send(
                AgentId("term"),
                .object([
                    "type": .string("input"),
                    "data": .string("echo hello-pty\n"),
                ]))

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

            _ = await kernel.send(
                AgentId("term"), .object(["type": .string("shutdown")]))
        }
    }

#endif
