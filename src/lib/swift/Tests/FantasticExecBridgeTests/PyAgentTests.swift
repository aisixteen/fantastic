// py_agent + folder bundles — python `py_agent/tests/test_py_agent.py`, case for
// case: real programs (python, speaking the canonical reference helper).

#if os(macOS)
    import FantasticJSON
    import FantasticKernel
import FantasticKernelState
    import Foundation
    import OrderedCollections
    import Testing

    @testable import FantasticExecBridge

    /// The canonical reference helper a program imports (python's own file).
    private let HELPER_URL = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        .deletingLastPathComponent()
        .appendingPathComponent("python/bundled_agents/io/py_agent/src/py_agent/fantastic_agent.py")

    private let ECHO = #"""
        import os, sys, time
        import fantastic_agent as fa

        def echo(p):
            fa.emit({"type": "echoed", "text": p.get("text")})
            return {"echo": p.get("text"), "pid": os.getpid()}

        def die(p):
            sys.stderr.write("dying on purpose\n")
            sys.stderr.flush()
            os._exit(3)

        def slow(p):
            time.sleep(30)
            return {"late": True}

        def chatty(p):
            sys.stdout.write("x" * 300000)
            sys.stdout.flush()
            return {"chatted": True}

        fa.serve({"echo": echo, "die": die, "slow": slow, "chatty": chatty})

        """#

    private let VERBS =
        "  echo: 'args: text:str. Returns {echo, pid}.'\n  die: 'No args. The program exits.'\n"
        + "  slow: 'No args. Sleeps 30 s.'\n  chatty: 'No args. Writes 300 kB to stdout.'\n"

    private final class Box: @unchecked Sendable {
        let dir: URL
        let kernel: Kernel
        init() {
            dir = FileManager.default.temporaryDirectory.appendingPathComponent(
                "pyagent_\(UUID().uuidString)")
            try? FileManager.default.createDirectory(
                at: dir.appendingPathComponent(".fantastic"), withIntermediateDirectories: true)
            let registry = BundleRegistry()
            registry.register(PY_AGENT_HANDLER_MODULE, PyAgentBundle())
            kernel = Kernel(storage: .inMemory, bundles: registry)
            let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
            kernel.register(root)
            kernel.setRoot(root)
        }
        deinit { try? FileManager.default.removeItem(at: dir) }
        var root: URL { dir.appendingPathComponent(".fantastic") }

        @discardableResult
        func bundle(_ name: String, program: String = ECHO, verbs: String = VERBS) throws -> URL {
            let folder = root.appendingPathComponent("bundles").appendingPathComponent(name)
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            try FileManager.default.copyItem(
                at: HELPER_URL, to: folder.appendingPathComponent("fantastic_agent.py"))
            try program.write(
                to: folder.appendingPathComponent("agent.py"), atomically: true, encoding: .utf8)
            try
                ("base: py_agent\nsentence: The \(name) agent.\ncommand: [python3, agent.py]\n"
                + "record: {inherit_env: [PATH]}\nverbs:\n\(verbs)")
                .write(
                    to: folder.appendingPathComponent("bundle.yaml"), atomically: true,
                    encoding: .utf8)
            return folder
        }

        func load() { Folders.loadBundles(kernel: kernel, root: root) }

        func call(_ target: String, _ payload: JSON) async -> JSON {
            await kernel.send([Item(AgentId(target), payload, timeout: 20)])[0].result
        }

        func create(_ hm: String, _ id: String) async -> JSON {
            await call(
                "kernel_state",
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string(hm), "id": .string(id),
                ]))
        }

        func delete(_ id: String) async {
            _ = await call(
                "kernel_state", .object(["type": .string("delete_agent"), "id": .string(id)]))
        }
    }

    private func gone(_ pid: Int32) async -> Bool {
        for _ in 0..<50 {
            if kill(pid, 0) != 0 { return true }
            try? await Task.sleep(nanoseconds: 100_000_000)
        }
        return false
    }

    @Suite("py_agent: a folder bundle's program, run by the kernel", .serialized)
    struct PyAgentTests {
        @Test func anInstanceRunsItsProgramAndAnswersTheBundlesVerbs() async throws {
            let b = Box()
            try b.bundle("sw_echoer")
            b.load()
            let r = await b.create("sw_echoer", "sw_e1")
            #expect(r["id"].asString == "sw_e1", "\(r)")
            let watcher = AgentId("_t_watch_sw")
            let inbox = b.kernel.ensureInbox(watcher)
            #expect(b.kernel.watch(src: AgentId("sw_e1"), watcher: watcher) == nil)
            let out = await b.call(
                "sw_e1", .object(["type": .string("echo"), "text": .string("hi")]))
            #expect(out["echo"].asString == "hi", "\(out)")
            #expect(out["pid"].asInt.map { $0 != Int64(getpid()) } == true)
            var seen = false
            for _ in 0..<40 where !seen {
                if let item = inbox.tryTake() {
                    seen =
                        item.payload["type"].asString == "echoed"
                        && item.payload["text"].asString == "hi"
                } else {
                    try? await Task.sleep(nanoseconds: 50_000_000)
                }
            }
            #expect(seen, "the program's emit must reach the instance's watchers")
            let rf = await b.call("sw_e1", .object(["type": .string("reflect")]))
            #expect(
                rf["base"].asString == "py_agent" && rf["program"].asString == "running", "\(rf)")
            #expect(rf["security"].asString?.contains("OS rights") == true)
            await b.delete("sw_e1")
        }

        @Test func docsAndTheCatalogReadBundleYaml() async throws {
            let b = Box()
            try b.bundle("sw_documented")
            b.load()
            let d = await b.call(
                "kernel", .object(["type": .string("docs"), "of": .string("sw_documented")]))
            #expect(d["verbs"]["echo"].asString == "args: text:str", "\(d)")
            let r = await b.call(
                "kernel",
                .object([
                    "type": .string("reflect"), "tree": .string("none"), "bundles": .string("all"),
                ]))
            let row = r["bundles"].asArray?.first { $0["name"].asString == "sw_documented" }
            #expect(row?["base"].asString == "py_agent", "\(String(describing: row))")
            #expect(row?["handler_module"].asString == "sw_documented")
            #expect(row?["sentence"].asString == "The sw_documented agent.")
            #expect(row?["folder"].asString == ".fantastic/bundles/sw_documented")
            #expect(row?["error"].isNull ?? false)
        }

        @Test func aProgramThatServesOtherVerbsThanDeclaredStopsSayingSo() async throws {
            let b = Box()
            try b.bundle("sw_liar", verbs: VERBS + "  extra: 'No args.'\n")
            b.load()
            _ = await b.create("sw_liar", "sw_l1")
            let out = await b.call(
                "sw_l1", .object(["type": .string("echo"), "text": .string("x")]))
            #expect(out["reason"].asString == "transport_closed", "\(out)")
            #expect(out["error"].asString?.contains("must match") == true, "\(out)")
            let rf = await b.call("sw_l1", .object(["type": .string("reflect")]))
            #expect(rf["program"].asString == "stopped")
            #expect(rf["program_error"].asString?.contains("must match") == true)
            await b.delete("sw_l1")
        }

        @Test func anExitSettlesTheCallAndNothingRespawns() async throws {
            let b = Box()
            try b.bundle("sw_mortal")
            b.load()
            _ = await b.create("sw_mortal", "sw_m1")
            let out = await b.call("sw_m1", .object(["type": .string("die")]))
            #expect(out["reason"].asString == "transport_closed", "\(out)")
            #expect(out["error"].asString?.contains("exited (code 3)") == true, "\(out)")
            try? await Task.sleep(nanoseconds: 200_000_000)
            let rf = await b.call("sw_m1", .object(["type": .string("reflect")]))
            #expect(rf["stderr_tail"].asString?.contains("dying on purpose") == true, "\(rf)")
            let again = await b.call(
                "sw_m1", .object(["type": .string("echo"), "text": .string("x")]))
            #expect(again["reason"].asString == "transport_closed")
            #expect(again["hint"].asString?.contains("reload_bundle") == true, "\(again)")
            await b.delete("sw_m1")
        }

        @Test func reloadRestartsTheInstanceAndAnswersTheInflightCall() async throws {
            let b = Box()
            let folder = try b.bundle("sw_reloaded")
            b.load()
            _ = await b.create("sw_reloaded", "sw_r1")
            let first = await b.call(
                "sw_r1", .object(["type": .string("echo"), "text": .string("a")]))
            let pending = Task { await b.call("sw_r1", .object(["type": .string("slow")])) }
            try await Task.sleep(nanoseconds: 500_000_000)
            try ECHO.replacingOccurrences(
                of: #""echo": p.get("text")"#, with: #""echo": "v2:" + p.get("text")"#
            )
            .write(to: folder.appendingPathComponent("agent.py"), atomically: true, encoding: .utf8)
            // what the root's `reload_bundle` does: re-load, then shutdown + boot each instance
            #expect(Folders.loadBundle(kernel: b.kernel, root: b.root, name: "sw_reloaded") == nil)
            let a = try #require(b.kernel.agent(AgentId("sw_r1")))
            _ = await b.kernel.bookkeeping(
                a, .object(["type": .string("shutdown"), "reason": .string("reloaded")]))
            _ = await b.kernel.bookkeeping(a, .object(["type": .string("boot")]))
            let late = await pending.value
            #expect(late["reason"].asString == "transport_closed", "\(late)")
            #expect(late["error"].asString?.contains("reloaded") == true, "\(late)")
            let second = await b.call(
                "sw_r1", .object(["type": .string("echo"), "text": .string("a")]))
            #expect(second["echo"].asString == "v2:a", "\(second)")
            #expect(second["pid"] != first["pid"])
            await b.delete("sw_r1")
        }

        /// A `bundles/<name>` that links OUTSIDE the running dir is refused by the
        /// clamp, never loaded (python reads through `fs`).
        @Test func aBundleFolderLinkingOutsideIsRefused() async throws {
            let b = Box()
            let real = try b.bundle("sw_escape_src")
            let away = FileManager.default.temporaryDirectory.appendingPathComponent(
                "pyagent_away_\(UUID().uuidString)")
            defer { try? FileManager.default.removeItem(at: away) }
            try FileManager.default.moveItem(at: real, to: away)
            try FileManager.default.createSymbolicLink(
                at: b.root.appendingPathComponent("bundles/sw_escape"), withDestinationURL: away)
            let err = Folders.loadBundle(kernel: b.kernel, root: b.root, name: "sw_escape")
            #expect(err?.contains("escapes root") == true, "\(String(describing: err))")
            #expect(b.kernel.bundles.get("sw_escape") == nil)
        }

        @Test func aFailedReloadKeepsThePreviousLoadRunning() async throws {
            let b = Box()
            let folder = try b.bundle("sw_kept")
            b.load()
            _ = await b.create("sw_kept", "sw_kp1")
            let first = await b.call(
                "sw_kp1", .object(["type": .string("echo"), "text": .string("a")]))
            // sentence gone
            try "base: py_agent\n".write(
                to: folder.appendingPathComponent("bundle.yaml"), atomically: true, encoding: .utf8)
            let err = Folders.loadBundle(kernel: b.kernel, root: b.root, name: "sw_kept")
            #expect(err?.contains("sentence") == true, "\(String(describing: err))")
            // the live instance runs the previous load, untouched; the catalog keeps it too
            let again = await b.call(
                "sw_kp1", .object(["type": .string("echo"), "text": .string("b")]))
            #expect(again["echo"].asString == "b" && again["pid"] == first["pid"], "\(again)")
            let row = b.kernel.bundles.folderRows().first { $0["name"].asString == "sw_kept" }
            #expect(
                row?["base"].asString == "py_agent" && row?["error"] == .null,
                "\(String(describing: row))")
            // and delete still reaches the program
            await b.delete("sw_kp1")
            #expect(await gone(Int32(first["pid"].asInt ?? -1)), "program still alive after delete")
        }

        @Test func deleteKillsTheProgram() async throws {
            let b = Box()
            try b.bundle("sw_killed")
            b.load()
            _ = await b.create("sw_killed", "sw_k1")
            let pid = await b.call(
                "sw_k1", .object(["type": .string("echo"), "text": .string("x")]))["pid"].asInt
            await b.delete("sw_k1")
            #expect(await gone(Int32(pid ?? -1)), "program still alive after delete")
        }

        @Test func cancellingTheProgramsTaskKillsTheProgram() async throws {
            let b = Box()
            try b.bundle("sw_cancelled")
            b.load()
            _ = await b.create("sw_cancelled", "sw_c1")
            let pid = await b.call(
                "sw_c1", .object(["type": .string("echo"), "text": .string("x")]))["pid"].asInt
            let ps = await b.call("kernel", .object(["type": .string("ps")]))
            let prog = try #require(
                ps["tasks"].asArray?.first {
                    $0["agent"].asString == "sw_c1" && $0["verb"].asString == "program"
                })
            _ = await b.call("kernel", .object(["type": .string("cancel"), "task_id": prog["id"]]))
            #expect(await gone(Int32(pid ?? -1)), "program still alive after cancel")
            let out = await b.call(
                "sw_c1", .object(["type": .string("echo"), "text": .string("x")]))
            #expect(out["reason"].asString == "transport_closed", "\(out)")
            #expect(out["error"].asString?.contains("cancelled") == true, "\(out)")
            await b.delete("sw_c1")
        }

        @Test func aChattyProgramDoesNotBlockOnItsOwnOutput() async throws {
            let b = Box()
            try b.bundle("sw_chatty")
            b.load()
            _ = await b.create("sw_chatty", "sw_ch1")
            let out = await b.call("sw_ch1", .object(["type": .string("chatty")]))
            #expect(out == .object(["chatted": .bool(true)]), "\(out)")
            await b.delete("sw_ch1")
        }

        @Test func aFolderThatCannotLoadIsListedWithItsError() async throws {
            let b = Box()
            let cases: [(String, String, String)] = [
                ("nobase", "sentence: x\n", "base: required"),
                ("badbase", "base: no_such_agent\nsentence: x\n", "known: py_agent"),
                ("nocmd", "base: py_agent\nsentence: x\nverbs: {a: 'b'}\n", "command: required"),
                (
                    "noverbs", "base: py_agent\nsentence: x\ncommand: [python3, a.py]\n",
                    "verbs: required"
                ),
                ("py_agent", "base: py_agent\nsentence: x\n", "built-in bundle"),
            ]
            for (name, yaml, _) in cases {
                let folder = b.root.appendingPathComponent("bundles").appendingPathComponent(name)
                try FileManager.default.createDirectory(
                    at: folder, withIntermediateDirectories: true)
                try yaml.write(
                    to: folder.appendingPathComponent("bundle.yaml"), atomically: true,
                    encoding: .utf8)
            }
            b.load()
            let r = await b.call(
                "kernel",
                .object([
                    "type": .string("reflect"), "tree": .string("none"), "bundles": .string("all"),
                ]))
            for (name, _, fragment) in cases {
                let row = r["bundles"].asArray?.first {
                    $0["name"].asString == name && !$0["folder"].isNull
                }
                #expect(
                    row?["error"].asString?.contains(fragment) == true,
                    "\(name): \(String(describing: row))")
            }
        }

        @Test func pyAgentItselfIsABaseNotAnAgent() async throws {
            let b = Box()
            let r = await b.create(PY_AGENT_HANDLER_MODULE, "sw_p0")
            #expect(r["reason"].asString == "invalid_meta", "\(r)")
            #expect(r["error"].asString?.contains("is a base") == true)
        }
    }
#endif
