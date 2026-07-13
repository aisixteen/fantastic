// FantasticJSRuntime tests — the in-process JavaScriptCore executor.
//
// Mirrors the Rust `fantastic-js-runtime` unit suite (echo round-trip,
// connector send→reply + self_id, stop/delete abort, boot rehydration) and
// adds the PLUMBED MIND: a JS-authored agent driving a (synchronous stub)
// backend through a tool-feedback cycle — the emergent-loop shape, on the JS
// substrate. All in-process + deterministic (no network, no disk).

import FantasticJSON
import FantasticJSRuntime
import FantasticKernel
import Foundation
import Testing

// MARK: - harness

private func freshKernel(extra: [(String, any AgentBundle)] = []) -> Kernel {
    let registry = BundleRegistry()
    registry.register("js_runtime.tools", JsRuntimeBundle())
    for (module, bundle) in extra { registry.register(module, bundle) }
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

/// Create a js_runtime agent carrying `code` on its record.
private func createScript(_ kernel: Kernel, id: String, code: String) async -> AgentId {
    let r = await kernel.send(
        AgentId("core"),
        .object([
            "type": .string("create_agent"),
            "handler_module": .string("js_runtime.tools"),
            "id": .string(id),
            "code": .string(code),
        ]))
    return AgentId(r["id"].asString ?? id)
}

/// Subscribe a synthetic watcher to `src`'s traffic; returns its inbox stream.
private func watchInbox(_ kernel: Kernel, _ src: AgentId) -> AsyncStream<JSON> {
    let probe = AgentId("probe_" + String(UUID().uuidString.prefix(6)))
    let stream = kernel.ensureInbox(probe)
    kernel.watch(src: src, watcher: probe)
    return stream
}

private actor EventSink {
    private(set) var events: [JSON] = []
    func add(_ e: JSON) { events.append(e) }
    func has(_ pred: @Sendable (JSON) -> Bool) -> Bool { events.contains(where: pred) }
}

/// Drain `stream` into a list until `pred` matches an event or `timeout` s pass.
private func waitForEvent(
    _ stream: AsyncStream<JSON>, timeout: Double = 5,
    where pred: @escaping @Sendable (JSON) -> Bool
) async -> [JSON] {
    let sink = EventSink()
    let consumer = Task { for await ev in stream { await sink.add(ev) } }
    let deadline = Date().addingTimeInterval(timeout)
    while Date() < deadline {
        if await sink.has(pred) { break }
        try? await Task.sleep(nanoseconds: 10_000_000)
    }
    consumer.cancel()
    return await sink.events
}

/// A minimal synchronous "backend" — answers `send` from a scripted response
/// list (each `send` pops the next). Stands in for a context-managed AI
/// backend so the JS mind's connector + feedback loop is tested deterministically.
private final class StubBackend: AgentBundle, @unchecked Sendable {
    let name = "stub_backend"
    private let lock = NSLock()
    private var scripts: [String]
    private(set) var calls = 0
    init(_ scripts: [String]) { self.scripts = scripts }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("stub backend"),
                "verbs": .object([:]),
            ])
        case "send":
            let r: String = lock.withLock {
                calls += 1
                return scripts.isEmpty ? "" : scripts.removeFirst()
            }
            return .object([
                "response": .string(r), "final": .string(r),
                "client_id": payload["client_id"],
            ])
        default:
            return .object(["error": .string("stub: unknown verb")])
        }
    }
}

// MARK: - tests

@Suite("FantasticJSRuntime")
struct JSRuntimeTests {

    @Test func readmePresentAndTitled() {
        let bundle = JsRuntimeBundle()
        #expect(bundle.readme?.contains("js_runtime") == true)
        #expect(bundle.name == "js_runtime")
    }

    @Test func echoScriptRoundTripsInboxToEmit() async throws {
        let code = #"""
            while (true) {
                const m = await kernel.on_message();
                if (m === null) break;
                await kernel.emit({ type: 'echo', text: m.text, client_id: m.client_id });
            }
            """#
        let kernel = freshKernel()
        let agent = await createScript(kernel, id: "echo", code: code)
        let started = await kernel.send(agent, .object(["type": .string("start")]))
        #expect(started["running"].asBool == true)

        let status = await kernel.send(agent, .object(["type": .string("status")]))
        #expect(status["running"].asBool == true)
        #expect(status["has_code"].asBool == true)

        let stream = watchInbox(kernel, agent)
        let ack = await kernel.send(
            agent,
            .object([
                "type": .string("ping"), "text": .string("hello js"),
                "client_id": .string("c1"),
            ]))
        #expect(ack["queued"].asBool == true)

        let events = await waitForEvent(stream) { $0["type"].asString == "echo" }
        let echo = events.first { $0["type"].asString == "echo" }
        #expect(echo != nil)
        #expect(echo?["text"].asString == "hello js")
        #expect(echo?["client_id"].asString == "c1")
    }

    /// A DETACHED entry point (`main().catch(console.error)` — the idiom most JS
    /// models write) must service its inbox for the agent's LIFETIME. The wrapper
    /// IIFE settles on the first microtask while the loop is still parked, so a
    /// naive `.then(__done)` tore the tool down before it read a single message.
    @Test func detachedEntryPointServesManyMessages() async throws {
        let code = #"""
            const kernel = require('kernel');
            async function main() {
                while (true) {
                    const m = await kernel.on_message();
                    if (m === null) break;
                    if (m.type === 'send') {
                        kernel.emit({ type: 'echo', text: m.text, client_id: m.client_id });
                    }
                }
            }
            main().catch(console.error);
            """#
        let kernel = freshKernel()
        let agent = await createScript(kernel, id: "detached", code: code)
        _ = await kernel.send(agent, .object(["type": .string("start")]))

        // Deliver several turns; the tool must answer EACH (not just the first).
        for n in 0..<4 {
            let stream = watchInbox(kernel, agent)
            let text = "turn \(n)"
            _ = await kernel.send(
                agent,
                .object([
                    "type": .string("send"), "text": .string(text),
                    "client_id": .string("c"),
                ]))
            let events = await waitForEvent(stream) {
                $0["type"].asString == "echo" && $0["text"].asString == text
            }
            #expect(
                events.contains { $0["type"].asString == "echo" && $0["text"].asString == text },
                "detached loop must still answer on \(text)")
        }
        let status = await kernel.send(agent, .object(["type": .string("status")]))
        #expect(status["running"].asBool == true, "detached tool still alive after many turns")
    }

    /// A COMPILE error in authored code must NOT leave a silent `running:true`
    /// zombie: the wrapper (incl. its `.then(__done)`) never installs, so the
    /// runtime itself must fire the terminal failure.
    @Test func compileErrorIsNotASilentZombie() async throws {
        let kernel = freshKernel()
        let agent = await createScript(kernel, id: "badsyntax", code: "const x = ;")
        let stream = watchInbox(kernel, agent)
        _ = await kernel.send(agent, .object(["type": .string("start")]))

        let events = await waitForEvent(stream) { $0["type"].asString == "job_done" }
        let done = events.first { $0["type"].asString == "job_done" }
        #expect(done != nil, "a compile error must emit job_done, not vanish")
        #expect(done?["ok"].asBool == false)
        let status = await kernel.send(agent, .object(["type": .string("status")]))
        #expect(status["running"].asBool == false, "no running:true zombie after a compile error")
    }

    @Test func scriptSendReachesAgentsAndReturnsReplyAndSelfId() async throws {
        let code = #"""
            const r = await kernel.send('core', { type: 'list_agents' });
            const n = (r.agents || []).length;
            await kernel.emit({ type: 'counted', agents: n, self_id: kernel.self_id });
            """#
        let kernel = freshKernel()
        let agent = await createScript(kernel, id: "probe_send", code: code)
        let stream = watchInbox(kernel, agent)
        _ = await kernel.send(agent, .object(["type": .string("start")]))

        let events = await waitForEvent(stream) { $0["type"].asString == "counted" }
        let counted = events.first { $0["type"].asString == "counted" }
        #expect(counted != nil)
        #expect((counted?["agents"].asInt ?? 0) >= 1)
        #expect(counted?["self_id"].asString == "probe_send")
    }

    @Test func stopAbortsAndDeleteCleansUp() async throws {
        let code = "while (true) { const m = await kernel.on_message(); if (m === null) break; }"
        let kernel = freshKernel()
        let agent = await createScript(kernel, id: "stopme", code: code)
        _ = await kernel.send(agent, .object(["type": .string("start")]))

        var status = await kernel.send(agent, .object(["type": .string("status")]))
        #expect(status["running"].asBool == true)

        let stopped = await kernel.send(agent, .object(["type": .string("stop")]))
        #expect(stopped["stopped"].asBool == true)

        status = await kernel.send(agent, .object(["type": .string("status")]))
        #expect(status["running"].asBool == false)

        // A further inbox verb errors — no running script to receive it.
        let mail = await kernel.send(agent, .object(["type": .string("ping")]))
        #expect(mail["queued"].asBool == nil)
        #expect(mail["error"].asString?.contains("no running script") == true)

        _ = await kernel.send(
            AgentId("core"),
            .object(["type": .string("delete_agent"), "id": .string("stopme")]))
        // (Delete drives on_delete → stop; nothing to assert beyond no crash.)
    }

    @Test func bootRehydratesFromCodeMeta() async throws {
        // The record's `code` meta IS the durable program: after a stop, `boot`
        // revives it from the record without re-supplying code. (In-memory stand-in
        // for the disk reboot the Rust suite proves with a fresh kernel.)
        let code = "while (true) { const m = await kernel.on_message(); if (m === null) break; }"
        let kernel = freshKernel()
        let agent = await createScript(kernel, id: "durable", code: code)
        _ = await kernel.send(agent, .object(["type": .string("start")]))
        _ = await kernel.send(agent, .object(["type": .string("stop")]))
        var status = await kernel.send(agent, .object(["type": .string("status")]))
        #expect(status["running"].asBool == false)
        #expect(status["has_code"].asBool == true)

        let booted = await kernel.send(agent, .object(["type": .string("boot")]))
        #expect(booted["started"].asBool == true)
        status = await kernel.send(agent, .object(["type": .string("status")]))
        #expect(status["running"].asBool == true)
    }

    @Test func plumbedMindDrivesBackendThroughFeedbackCycle() async throws {
        // A JS-authored mind drives a (synchronous) backend through a tool
        // feedback cycle: send → "CALL core list_agents" → dispatch → feed
        // RESULT back → "FINAL: …" → emit say + done. The tool LOOP is the
        // mind's (emergent), not the substrate's.
        let mindJS = #"""
            const SYSTEM = "drive a kernel: CALL <target> <verb>, or FINAL: <answer>";
            while (true) {
                const m = await kernel.on_message();
                if (m === null) break;
                if (m.type !== 'send') continue;
                let text = m.text;
                const cid = m.client_id;
                for (let i = 0; i < 4; i++) {
                    const reply = await kernel.send('the_ai', {
                        type: 'send', text: text, system_prompt: SYSTEM, client_id: cid
                    });
                    const resp = (reply && (reply.response || reply.error)) || '';
                    const mm = /^CALL (\S+) (\S+)/.exec(resp);
                    if (mm) {
                        const r = await kernel.send(mm[1], { type: mm[2] });
                        text = 'RESULT: ' + JSON.stringify(r);
                    } else {
                        const ans = resp.indexOf('FINAL: ') === 0 ? resp.slice(7) : resp;
                        await kernel.emit({ type: 'say', text: ans, client_id: cid });
                        await kernel.emit({ type: 'done', client_id: cid });
                        break;
                    }
                }
            }
            """#
        let stub = StubBackend(["CALL core list_agents", "FINAL: the kernel is populated"])
        let kernel = freshKernel(extra: [("stub_backend.tools", stub)])
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("stub_backend.tools"),
                "id": .string("the_ai"),
            ]))
        // create_agent auto-boots the script from its `code` meta — the mind is
        // live immediately (the durable-agent boot path is covered separately by
        // bootRehydratesFromCodeMeta).
        let mind = await createScript(kernel, id: "the_mind", code: mindJS)
        let pre = await kernel.send(mind, .object(["type": .string("status")]))
        #expect(pre["running"].asBool == true)

        let stream = watchInbox(kernel, mind)
        let ack = await kernel.send(
            mind,
            .object([
                "type": .string("send"), "text": .string("what lives here?"),
                "client_id": .string("probe"),
            ]))
        #expect(ack["queued"].asBool == true)

        let events = await waitForEvent(stream) { $0["type"].asString == "done" }
        let say = events.first { $0["type"].asString == "say" }
        let done = events.first { $0["type"].asString == "done" }
        #expect(done != nil, "the mind must finish the turn with a done event")
        #expect(say?["client_id"].asString == "probe")
        #expect((say?["text"].asString ?? "").contains("populated"))
        // The feedback cycle really ran: TWO backend passes (act → result → answer).
        #expect(stub.calls == 2)
    }
}
