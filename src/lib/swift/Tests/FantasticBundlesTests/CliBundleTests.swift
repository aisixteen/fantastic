// cli — the render sink's contract. Twins of python's
// `bundled_agents/cli/tests/test_cli.py` and rust's `fantastic-cli-bundle`
// tests. Python captures stdout; here the LINES are pinned through the pure
// render functions (stdout is process-wide and the suites run in parallel),
// and the DISPATCH — which verbs are handled, which are dropped, and that
// reflect advertises exactly what it accepts — through the kernel.

import FantasticJSON
import FantasticKernel
import Foundation
import Testing

@testable import FantasticCliBundle

@Suite("cli (terminal render sink)")
struct CliBundleTests {
    private func seeded() async -> Kernel {
        let registry = BundleRegistry()
        registry.register(CliBundle.handlerModule, CliBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": .string(CliBundle.handlerModule), "id": "cli",
            ])])[0].result
        return kernel
    }

    private func call(_ k: Kernel, _ payload: JSON) async -> JSON {
        await k.send([Item(AgentId("cli"), payload)])[0].result
    }

    // ─── dispatch (rust twins) ──────────────────────────────────

    @Test func reflectAdvertisesExactlyWhatItAccepts() async {
        let kernel = await seeded()
        let r = await call(kernel, ["type": "reflect"])
        #expect(r["id"].asString == "cli")
        #expect(r["sentence"].asString == "Terminal renderer. Prints incoming events to stdout.")
        // Python's key order; the substrate appends its frame after them.
        #expect(
            r.asObject?.keys.starts(with: ["id", "sentence", "verbs", "accepts"]) == true, "\(r)")
        let accepts = (r["accepts"].asArray ?? []).compactMap { $0.asString }
        #expect(accepts == ["token", "done", "say", "error", "status", "context"])
        // The list and the dispatch must not drift: a verb advertised but not
        // rendered is a promise this sink does not keep.
        for verb in CliBundle.accepts {
            #expect(accepts.contains(verb), "missing \(verb)")
            #expect(r["verbs"][verb].asString != nil, "\(verb) accepted but undocumented")
        }
        // Python's VERBS table, doc strings verbatim.
        #expect(
            Set((r["verbs"].asObject ?? [:]).keys)
                == ["reflect", "token", "done", "say", "error", "status", "context", "intro_booting", "booted"])
        #expect(r["verbs"]["token"].asString == "args: text:str. Writes to stdout (no newline). Returns None.")
    }

    @Test func anUnknownVerbIsDroppedNotRefused() async {
        // A render surface that answered `unknown verb` would turn a cosmetic gap
        // into a FAILED SEND for whoever was streaming to it.
        let kernel = await seeded()
        let r = await call(kernel, ["type": "garbage"])
        #expect(r.isNull, "\(r.serialize())")
    }

    @Test func everyAcceptedVerbAnswersNullRatherThanAnError() async {
        let kernel = await seeded()
        for verb in CliBundle.accepts {
            let r = await call(kernel, ["type": .string(verb), "text": "x"])
            #expect(r.isNull, "\(verb) refused: \(r.serialize())")
        }
        for verb in ["intro_booting", "booted"] {
            let r = await call(kernel, ["type": .string(verb)])
            #expect(r.isNull, "\(verb) refused: \(r.serialize())")
        }
    }

    // ─── rendering (python twins, on the lines) ─────────────────

    @Test func sayWithSourcePrefix() {
        let line = CliBundle.renderSay(["type": "say", "text": "hello", "source": "agent_x"])
        #expect(line == "  [agent_x] hello")
    }

    @Test func sayWithoutSource() {
        #expect(CliBundle.renderSay(["type": "say", "text": "no-source"]) == "  no-source")
    }

    @Test func errorWritesErrorPrefix() {
        #expect(CliBundle.renderError(["type": "error", "text": "boom"]) == "  ERROR: boom")
    }

    @Test func statusRendersPhaseMarkers() throws {
        // queued, thinking (regular + rate-limit), tool_calling entry+exit each
        // produce one specific line.
        let lines: [String?] = [
            try CliBundle.renderStatus([
                "type": "status", "source": "ollama_xx", "phase": "queued",
                "detail": ["ahead": .integer(2), "send_id": "abc"],
            ]),
            try CliBundle.renderStatus([
                "type": "status", "source": "ollama_xx", "phase": "thinking",
                "detail": ["send_id": "abc"],
            ]),
            try CliBundle.renderStatus([
                "type": "status", "source": "nvidia_nim_xx", "phase": "thinking",
                "detail": ["waiting_on": "rate_limit", "wait_s": .integer(5)],
            ]),
            try CliBundle.renderStatus([
                "type": "status", "source": "ollama_xx", "phase": "tool_calling",
                "detail": [
                    "tool": [
                        "call_id": "c1", "target": "kernel_state", "verb": "list_agents",
                        "args": ["target_id": "kernel_state", "payload": ["type": "list_agents"]],
                    ]
                ],
            ]),
            try CliBundle.renderStatus([
                "type": "status", "source": "ollama_xx", "phase": "tool_calling",
                "detail": [
                    "tool": [
                        "call_id": "c1", "target": "kernel_state", "verb": "list_agents",
                        "args": [:],
                        "reply_preview": "{\"agents\":[{\"id\":\"kernel_state\"}]}",
                    ]
                ],
            ]),
        ]
        let out = lines.compactMap { $0 }
        #expect(out.count == 5, "expected 5 lines, got \(out.count): \(out)")
        #expect(out[0] == "  [ollama_xx] queued (2 ahead)")
        #expect(out[1] == "  [ollama_xx] thinking…")
        #expect(out[2] == "  [nvidia_nim_xx] rate-limited; waiting 5s")
        #expect(
            out[3]
                == "  [ollama_xx] → list_agents(kernel_state)  {\"target_id\": \"kernel_state\", \"payload\": {\"type\": \"list_agents\"}}")
        #expect(out[4] == "  [ollama_xx] ← list_agents(kernel_state)  {\"agents\":[{\"id\":\"kernel_state\"}]}")
    }

    @Test func statusStreamingAndDoneAreSilent() throws {
        #expect(try CliBundle.renderStatus(["type": "status", "phase": "streaming", "detail": [:]]) == nil)
        #expect(
            try CliBundle.renderStatus(["type": "status", "phase": "done", "detail": ["reason": "ok"]])
                == nil)
    }

    @Test func statusWithoutSourceUsesTheBarePrefix() throws {
        #expect(
            try CliBundle.renderStatus(["type": "status", "phase": "queued", "detail": [:]])
                == "  queued (0 ahead)")
    }

    @Test func contextRendersTheProtocolNotices() throws {
        #expect(
            try CliBundle.renderContext([
                "type": "context", "source": "ai", "phase": "compacted",
                "detail": ["strategy": "fold", "dropped_messages": .integer(4)],
            ]) == "  [ai] ⟲ context compacted (fold, 4 dropped; recall to page back)")
        #expect(
            try CliBundle.renderContext([
                "type": "context", "phase": "too_small", "detail": ["hint": "raise num_ctx"],
            ]) == "  ✗ context too small — raise num_ctx")
        #expect(try CliBundle.renderContext(["type": "context", "phase": "other"]) == nil)
    }

    // ─── python `str()`, python defaults, python truthiness (A9.7) ───

    @Test func statusValuesRenderLikePythonStr() throws {
        // Python drops every one of these into an f-string, so `str()` — not
        // JSON — decides the bytes: None/True/False are words, a container is
        // python's repr, and a whole float keeps its `.0`.
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "phase": "queued", "detail": ["ahead": .null],
            ]) == "  queued (None ahead)")
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "phase": "queued", "detail": ["ahead": [1, "a"]],
            ]) == "  queued ([1, 'a'] ahead)")
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "phase": "queued", "detail": ["ahead": .double(1.0)],
            ]) == "  queued (1.0 ahead)")
        // A DEFAULT IS BY KEY PRESENCE. `detail.get("wait_s", "?")` hands back a
        // present `null` as `None`; only a missing key becomes `?`.
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "phase": "thinking",
                "detail": ["waiting_on": "rate_limit"],
            ]) == "  rate-limited; waiting ?s")
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "phase": "thinking",
                "detail": ["waiting_on": "rate_limit", "wait_s": .null],
            ]) == "  rate-limited; waiting Nones")
        // `verb` / `target` are f-string values too, and a missing one is "".
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "phase": "tool_calling",
                "detail": ["tool": ["verb": .integer(7), "target": .null, "args": ["a": 1]]],
            ]) == "  → 7(None)  {\"a\": 1}")
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "phase": "tool_calling", "detail": ["tool": [:]],
            ]) == "  → ()  {}")
        // ENTRY vs EXIT is the KEY being there, not its value: a null preview is
        // still an exit, rendered as `None`.
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "phase": "tool_calling",
                "detail": ["tool": ["verb": "v", "target": "t", "reply_preview": .null]],
            ]) == "  ← v(t)  None")
        // `tool.get("args") or {}` is TRUTHINESS: every falsy args is `{}`.
        let falsies: [JSON] = [.null, .array([]), .object([:]), .integer(0), .string(""), false]
        for falsy in falsies {
            #expect(
                try CliBundle.renderStatus([
                    "type": "status", "phase": "tool_calling",
                    "detail": ["tool": ["verb": "v", "target": "t", "args": falsy]],
                ]) == "  → v(t)  {}", "\(falsy.serialize())")
        }
        // A truthy non-dict args is dumped as it is — python's `json.dumps`
        // takes a list too.
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "phase": "tool_calling",
                "detail": ["tool": ["verb": "v", "target": "t", "args": [1, 2]]],
            ]) == "  → v(t)  [1, 2]")
        // `source` is truthiness as well: `0` / `""` / `false` take the bare
        // prefix, a truthy non-string renders through `str()`.
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "source": .integer(0), "phase": "queued", "detail": [:],
            ]) == "  queued (0 ahead)")
        #expect(
            try CliBundle.renderStatus([
                "type": "status", "source": .integer(7), "phase": "queued", "detail": [:],
            ]) == "  [7] queued (0 ahead)")
        // context's own defaults and values follow the same three rules.
        #expect(
            try CliBundle.renderContext(["type": "context", "phase": "compacted", "detail": [:]])
                == "  ⟲ context compacted (?, 0 dropped; recall to page back)")
        #expect(
            try CliBundle.renderContext([
                "type": "context", "phase": "compacted",
                "detail": ["strategy": .null, "dropped_messages": .bool(true)],
            ]) == "  ⟲ context compacted (None, True dropped; recall to page back)")
        #expect(
            try CliBundle.renderContext([
                "type": "context", "phase": "too_small", "detail": ["hint": .bool(true)],
            ]) == "  ✗ context too small — True")
        #expect(
            try CliBundle.renderContext(["type": "context", "phase": "too_small", "detail": [:]])
                == "  ✗ context too small — ")
    }

    @Test func sayAndErrorRenderValuesWithPythonStr() {
        // `say` keeps its own trailing space in the prefix; both texts are
        // f-string values, so a non-string is `str(x)`, never dropped.
        #expect(CliBundle.renderSay(["type": "say", "text": .null]) == "  None")
        #expect(
            CliBundle.renderSay(["type": "say", "text": ["a": 1], "source": .integer(7)])
                == "  [7] {'a': 1}")
        #expect(CliBundle.renderSay(["type": "say", "text": "t", "source": .bool(false)]) == "  t")
        #expect(CliBundle.renderError(["type": "error", "text": .integer(5)]) == "  ERROR: 5")
        #expect(CliBundle.renderError(["type": "error"]) == "  ERROR: ")
    }

    @Test func aRaisingRenderIsStampedWithPythonsExceptionText() async {
        // Python does not defend these: `sys.stdout.write` refuses a non-str and
        // `.get` on a truthy non-dict is an AttributeError, so the DISPATCH
        // fails and the kernel stamps `"<ExcType>: <msg>"`.
        let kernel = await seeded()
        let bad = await call(kernel, ["type": "token", "text": .integer(5)])
        #expect(bad["error"].asString == "TypeError: write() argument must be str, not int")
        #expect(bad["raised"].asBool == true)
        let none = await call(kernel, ["type": "token", "text": .null])
        #expect(none["error"].asString == "TypeError: write() argument must be str, not None")
        let truthyDetail = await call(kernel, ["type": "status", "phase": "queued", "detail": 5])
        #expect(
            truthyDetail["error"].asString
                == "AttributeError: 'int' object has no attribute 'get'")
        let toolPayload: JSON = [
            "type": "status", "phase": "tool_calling", "detail": ["tool": "x"],
        ]
        let truthyTool = await call(kernel, toolPayload)
        #expect(
            truthyTool["error"].asString
                == "AttributeError: 'str' object has no attribute 'get'")
        let ctx = await call(kernel, ["type": "context", "phase": "compacted", "detail": [1]])
        #expect(ctx["error"].asString == "AttributeError: 'list' object has no attribute 'get'")
        // A FALSY non-dict detail is `{}`, not a raise — `or {}` swallows it.
        let falsyDetail = await call(kernel, ["type": "status", "phase": "queued", "detail": 0])
        #expect(falsyDetail.isNull, "\(falsyDetail.serialize())")
    }

    // ─── two-phase PTY intro ────────────────────────────────────

    @Test func introBootingPrintsControlPlaneMap() async {
        // First PTY push (pre-boot): identity + the pull/push plane map, port-free.
        let kernel = await seeded()
        let out = CliBundle.introBootingLines(kernel).joined(separator: "\n")
        #expect(out.contains("booting"))
        #expect(out.contains("send(<id>"))
        #expect(out.contains("PULL") && out.contains("PUSH") && out.contains("REACH"))
        #expect(out.contains("reflect readme=true"))
        // identity line carries deployment context + the root id + this pid
        #expect(out.contains("swift") && out.contains("env=") && out.contains("root=kernel_state"))
        #expect(out.contains("pid \(ProcessInfo.processInfo.processIdentifier)"))
    }

    @Test func bootedIsADumbSinkNoTreeInspection() {
        // Final PTY push: the kernel's 'all booted' close — a fixed line, never a
        // tree walk for ports/surfaces (each agent announces its own on boot).
        #expect(CliBundle.bootedLine.contains("all booted"))
        #expect(CliBundle.bootedLine.contains("reflect readme=true"))
    }

    @Test func readmeIsPythonsVerbatim() {
        let readme = CliBundle().readme ?? ""
        #expect(readme.hasPrefix("# cli — stdout renderer\n"))
        #expect(readme.contains("A DUMB SINK for the PTY intro too"))
    }
}
