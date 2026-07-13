// js_runtime — a general-purpose IN-PROCESS JavaScript executor bundle
// (JavaScriptCore, an Apple system framework — no external deps). NOT
// agentic: this bundle knows nothing about AI, loops, or control flow. It
// gives caller-authored JS a persistent, addressable, bootable home — the
// SAME contract as the Rust `fantastic-js-runtime` (QuickJS) and the Python
// `python_runtime`, on Apple's JS engine:
//
//   - the agent record's `code` meta IS the program (persists → survives
//     reboot; `boot` re-starts it),
//   - the script's ONLY capability is the kernel connector:
//       await kernel.send(target, payload)  → reply object
//       await kernel.emit(payload)          → emit an event on THIS agent
//       await kernel.on_message()           → next inbox payload (null on death)
//       kernel.self_id                      → this agent's id
//     No fs, no net, no process — sandbox by omission.
//   - any verb this bundle doesn't own is queued for the script
//     (`{queued:true}` ack) and delivered via `on_message()`.
//
// Authored code runs inside an async IIFE (so top-level `await` works). The
// Swift↔JS bridge crosses as JSON STRINGS (a tiny prelude re-exposes it as
// plain objects), mirroring the Rust bundle exactly.
//
// The async bridge (the interesting part): JavaScriptCore has a native
// Promise + microtask queue that it DRAINS after every JS↔native boundary
// return. So each connector native returns a hand-rolled `JSValue` Promise;
// the async kernel call runs on a Swift `Task` (off the JS queue) and, when
// it completes, resolves that Promise back ON the job's serial queue —
// JSContext is single-threaded, so ALL context access is funnelled onto one
// `DispatchQueue` per job. Resolving advances the awaiting `async` script.

#if canImport(JavaScriptCore)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import JavaScriptCore

    public let HANDLER_MODULE = "js_runtime.tools"

    /// The JS-side prelude: re-wrap the string bridge as a plain-object API.
    /// Byte-for-byte the same surface as the Rust bundle's `PRELUDE`.
    private let PRELUDE = """
        globalThis.kernel = {
            self_id: __self_id,
            send: async (target, payload) =>
                JSON.parse(await __send(target, JSON.stringify(payload === undefined ? {} : payload))),
            emit: async (payload) => { await __emit(JSON.stringify(payload === undefined ? {} : payload)); },
            on_message: async () => {
                const s = await __on_message();
                return (s === null || s === undefined) ? null : JSON.parse(s);
            },
            reflect: async (target) =>
                JSON.parse(await __send(target === undefined ? 'kernel' : target, JSON.stringify({ type: 'reflect' }))),
        };
        globalThis.require = (name) => {
            if (name === 'kernel') return globalThis.kernel;
            throw new Error("require: no module '" + name +
                "' — the only capability is the kernel connector (already the global `kernel`).");
        };
        if (typeof globalThis.console === 'undefined') {
            const _noop = () => {};
            globalThis.console = { log: _noop, error: _noop, warn: _noop, info: _noop, debug: _noop };
        }
        """

    public final class JsRuntimeBundle: AgentBundle, @unchecked Sendable {
        public let name = "js_runtime"
        public init() {}

        /// One living script per agent. ALL `context` access happens on `queue`
        /// (JSContext is not thread-safe). `pendingMessages` buffers inbox
        /// payloads that arrived before the script awaited them; `pendingResolvers`
        /// holds the resolve-fn of a promise the script is currently awaiting via
        /// `on_message()`. Both touched ONLY on `queue`, so they need no lock.
        /// `@unchecked Sendable`: every field is touched ONLY on `queue` (the
        /// serial JS queue), so cross-executor capture (a resolver-Task hopping
        /// back to `queue`) is race-free by discipline, not by the type system.
        private final class Job: @unchecked Sendable {
            let queue: DispatchQueue
            let context: JSContext
            var pendingMessages: [String] = []
            var pendingResolvers: [JSValue] = []
            var dead = false
            init(queue: DispatchQueue, context: JSContext) {
                self.queue = queue
                self.context = context
            }
        }

        /// Carries a non-Sendable JSValue (a promise resolver) across the
        /// cooperative-pool `Task` boundary back to the JS queue, where it is the
        /// only thing that touches it. Safe by the same queue discipline as `Job`.
        private final class Unchecked<T>: @unchecked Sendable {
            let value: T
            init(_ value: T) { self.value = value }
        }

        /// Guards the `jobs` map only (accessed from `handle` on arbitrary
        /// executors). Never held across an `await` or across queue work.
        private let lock = NSLock()
        private var jobs: [String: Job] = [:]

        public var readme: String? {
            """
            js_runtime — a general-purpose in-process JavaScript executor (JavaScriptCore). \
            The agent record's `code` meta IS the program; `boot` rehydrates it. The script's \
            only capability is the kernel connector: `await kernel.send(target, payload)`, \
            `await kernel.emit(payload)`, `await kernel.reflect(target?)`, `await kernel.on_message()`, \
            `kernel.self_id`. Any verb this bundle doesn't own is queued to the script's inbox \
            ({queued:true} ack) and delivered via on_message(). Verbs: reflect · boot · start · stop · status · shutdown.

            Authoring a durable tool — create_agent handler_module=js_runtime.tools id=<name> \
            code=<js source>; the record's `code` IS the tool (persists, boots on reboot). \
            USAGE PATTERNS: \
            (1) on_message is a PULL for js — loop `while(true){ const m = await kernel.on_message(); if(m===null) break; if(m.type!=='send') continue; /* handle turn */ }`; NEVER pass a callback. \
            (2) `code` is ONE JSON string — keep each string literal's content on one line (no raw line break inside a string); escape as needed. \
            (3) every turn MUST `kernel.emit` a `say` (the answer, tagged with the caller's client_id) THEN a `done` (also tagged client_id). \
            (4) `delete_agent <name>` is how you remove your own tool.
            """
        }

        // MARK: - dispatch

        public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
            let verb = payload["type"].asString ?? ""
            switch verb {
            case "reflect":
                return reflectReply(agentId, kernel)
            case "boot":
                // Rehydration: revive the persisted program if not already running.
                if let code = metaCode(agentId, kernel), !running(agentId) {
                    let started = startJob(agentId, code: code, kernel: kernel)
                    return .object(["started": .bool(started)])
                }
                return .null
            case "start":
                let inline = payload["code"].asString
                guard let code = inline ?? metaCode(agentId, kernel) else {
                    return .object([
                        "error": .string(
                            "start: no `code` — pass it or set it on the agent record")
                    ])
                }
                // Durability: persist inline code onto the record so `boot` can
                // rehydrate it (mirrors the Python python_runtime; the record IS
                // the durable program).
                if inline != nil, let agent = kernel.agent(agentId) {
                    _ = agent.updateMeta(["code": .string(code)])
                    await kernel.persistRecord(agent)
                }
                // Report the REAL outcome — never an optimistic `running:true`. If
                // the JS context could not be created the job was NOT installed; fail
                // loud rather than lie (no-fallbacks rule).
                guard startJob(agentId, code: code, kernel: kernel) else {
                    return .object([
                        "started": .bool(false),
                        "error": .string(
                            "start: could not create a JavaScript context (JavaScriptCore returned nil)"
                        ),
                    ])
                }
                return .object(["started": .bool(true), "running": .bool(running(agentId))])
            case "stop":
                return .object(["stopped": .bool(stopJob(agentId))])
            case "status":
                return .object([
                    "running": .bool(running(agentId)),
                    "has_code": .bool(metaCode(agentId, kernel) != nil),
                ])
            case "shutdown":
                _ = stopJob(agentId)
                return .null
            default:
                // Addressable inbox: any verb this bundle doesn't own is queued to
                // the running script, delivered via `await kernel.on_message()`.
                if deliver(agentId, payload) {
                    return .object(["queued": .bool(true)])
                }
                return .object([
                    "error": .string(
                        "\(agentId.value): no running script to receive '\(verb)' — start it first (boot/start)"
                    )
                ])
            }
        }

        public func onDelete(agentId: AgentId, kernel: Kernel) async throws { _ = stopJob(agentId) }
        public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            _ = stopJob(agentId)
        }

        // MARK: - record helpers

        private func metaCode(_ agentId: AgentId, _ kernel: Kernel) -> String? {
            kernel.agent(agentId)?.metaValue(forKey: "code")?.asString
        }

        private func running(_ agentId: AgentId) -> Bool {
            lock.lock()
            defer { lock.unlock() }
            guard let job = jobs[agentId.value] else { return false }
            return !job.dead
        }

        // MARK: - lifecycle

        /// (Re)start the script. One living script per agent — an existing one is
        /// aborted first (restart semantics, matching Rust/Python). Returns whether
        /// the job was installed: `false` iff the JS context could not be created.
        @discardableResult
        private func startJob(_ agentId: AgentId, code: String, kernel: Kernel) -> Bool {
            _ = stopJob(agentId)
            guard let context = JSContext() else { return false }
            let queue = DispatchQueue(label: "js.\(agentId.value)")
            let job = Job(queue: queue, context: context)
            lock.lock()
            jobs[agentId.value] = job
            lock.unlock()

            let selfId = agentId.value
            queue.async { [weak self, weak kernel, weak job] in
                guard let self, let job else { return }
                let ctx = job.context
                // Capture a COMPILE/parse error. A runtime error inside the async IIFE
                // rejects its promise and routes to `.catch → __done(false)`; only a
                // failure to PARSE the wrapper reaches the exception handler — and that
                // failure means the wrapper (incl. its `.then(__done)`) never installed,
                // so __done could never fire. Without catching it, a syntactically-bad
                // `code` left a silent `running:true` zombie.
                let compileError = Unchecked(NSMutableString())
                ctx.exceptionHandler = { _, exc in
                    if let s = exc?.toString() { compileError.value.setString(s) }
                }
                ctx.setObject(selfId, forKeyedSubscript: "__self_id" as NSString)

                // __send(target, payloadJSON) -> Promise<replyJSON>. The async kernel
                // call runs on a Task (off this queue); it resolves back here.
                let sendBlock: @convention(block) (String, String) -> JSValue? = {
                    [weak self, weak kernel, weak job] target, payloadJSON in
                    guard let self, let job else { return nil }
                    return self.makePromise(job) { resolve, _ in
                        let boxed = Unchecked(resolve)
                        let payload = (try? JSON.parse(payloadJSON)) ?? .object([:])
                        Task { [weak kernel, weak job] in
                            let reply = await (kernel?.send(AgentId(target), payload)) ?? .null
                            guard let job else { return }
                            job.queue.async {
                                guard !job.dead else { return }
                                boxed.value.call(withArguments: [reply.serialize()])
                            }
                        }
                    }
                }
                ctx.setObject(sendBlock, forKeyedSubscript: "__send" as NSString)

                // __emit(payloadJSON) -> Promise<void>. One-arg emit on the OWNER.
                let emitBlock: @convention(block) (String) -> JSValue? = {
                    [weak self, weak kernel, weak job] payloadJSON in
                    guard let self, let job else { return nil }
                    return self.makePromise(job) { resolve, _ in
                        let boxed = Unchecked(resolve)
                        let payload = (try? JSON.parse(payloadJSON)) ?? .object([:])
                        Task { [weak kernel, weak job] in
                            await kernel?.emit(AgentId(selfId), payload)
                            guard let job else { return }
                            job.queue.async {
                                guard !job.dead else { return }
                                boxed.value.call(withArguments: [])
                            }
                        }
                    }
                }
                ctx.setObject(emitBlock, forKeyedSubscript: "__emit" as NSString)

                // __on_message() -> Promise<payloadJSON | null>. Resolves with a
                // buffered payload if one waits, else parks the resolver until a
                // turn is delivered (or `null` when the script is stopped/deleted).
                let onMsgBlock: @convention(block) () -> JSValue? = { [weak self, weak job] in
                    guard let self, let job else { return nil }
                    return self.makePromise(job) { resolve, _ in
                        // Runs on the JS queue (JS called us there).
                        if job.dead {
                            resolve.call(withArguments: [JSValue(nullIn: job.context) as Any])
                        } else if !job.pendingMessages.isEmpty {
                            resolve.call(withArguments: [job.pendingMessages.removeFirst()])
                        } else {
                            job.pendingResolvers.append(resolve)
                        }
                    }
                }
                ctx.setObject(onMsgBlock, forKeyedSubscript: "__on_message" as NSString)

                // __done(ok, errString?) — the script ran to completion (or threw).
                let doneBlock: @convention(block) (Bool, JSValue?) -> Void = {
                    [weak self, weak kernel, weak job] ok, err in
                    guard let self, let job else { return }
                    // Already terminated EXTERNALLY (a `stop`, or a `start`/`boot`
                    // restart that superseded this incarnation). `stopJob` set
                    // `dead` and — on a restart — installed a REPLACEMENT job under
                    // the same id. This late completion is that dead incarnation's
                    // loop breaking on the null `on_message`; it must NOT touch the
                    // registry (it would evict the live replacement) nor emit a
                    // `job_done` for an already-torn-down job. Just stop here.
                    if job.dead { return }
                    // A DETACHED entry point (`main().catch(...)` — the idiom most
                    // JS-trained models write) settles the wrapper IIFE on the first
                    // microtask while the real message loop is still parked on
                    // `on_message`. That is a LIVING tool, not a finished script — do
                    // NOT tear it down on a successful settle while a resolver is
                    // parked. Genuine completion (loop exited → no parked resolver) and
                    // any error still end the job. (Runs on the JS queue → safe read.)
                    if ok, !job.pendingResolvers.isEmpty { return }
                    let errStr = (err?.isString == true) ? err?.toString() : nil
                    // Evict by IDENTITY, never blindly by key: only ever remove
                    // OURSELVES so a stale callback can never clobber a replacement.
                    self.lock.lock()
                    if self.jobs[selfId] === job { self.jobs.removeValue(forKey: selfId) }
                    self.lock.unlock()
                    job.dead = true
                    let event: JSON =
                        errStr == nil
                        ? .object([
                            "type": .string("job_done"), "source": .string(selfId),
                            "ok": .bool(ok),
                        ])
                        : .object([
                            "type": .string("job_done"), "source": .string(selfId),
                            "ok": .bool(ok), "error": .string(errStr ?? ""),
                        ])
                    Task { [weak kernel] in await kernel?.emit(AgentId(selfId), event) }
                }
                ctx.setObject(doneBlock, forKeyedSubscript: "__done" as NSString)

                // Wrap the authored code in an async IIFE (top-level await), and
                // funnel completion/rejection to __done.
                let wrapped =
                    PRELUDE + "\n(async () => {\n" + code + "\n})()"
                    + ".then(() => __done(true, null), (e) => __done(false, String(e)));"
                ctx.evaluateScript(wrapped)
                // If the exception handler tripped during eval, the wrapped script
                // failed to PARSE (a runtime error inside the async IIFE rejects its
                // promise and routes to `.catch → __done`, never here). That means the
                // `.then(__done)` never installed, so fire the terminal failure here
                // instead of leaving a running:true zombie.
                if compileError.value.length > 0 {
                    let msg = compileError.value as String
                    doneBlock(false, JSValue(object: "compile error: \(msg)", in: ctx))
                }
            }
            return true
        }

        /// Abort the running script. Resolves any parked `on_message()` with
        /// `null` so an `await` loop breaks cleanly. Returns whether one was live.
        @discardableResult
        private func stopJob(_ agentId: AgentId) -> Bool {
            lock.lock()
            let job = jobs.removeValue(forKey: agentId.value)
            lock.unlock()
            guard let job else { return false }
            let wasLive = !job.dead
            job.queue.async { [job] in
                job.dead = true
                let parked = job.pendingResolvers
                job.pendingResolvers.removeAll()
                for r in parked { r.call(withArguments: [JSValue(nullIn: job.context) as Any]) }
            }
            return wasLive
        }

        /// Deliver an inbox payload to the running script: resolve a parked
        /// `on_message()` if one waits, else buffer it. Returns false if no job.
        private func deliver(_ agentId: AgentId, _ payload: JSON) -> Bool {
            lock.lock()
            let job = jobs[agentId.value]
            lock.unlock()
            guard let job else { return false }
            let s = payload.serialize()
            job.queue.async { [job] in
                guard !job.dead else { return }
                if !job.pendingResolvers.isEmpty {
                    job.pendingResolvers.removeFirst().call(withArguments: [s])
                } else {
                    job.pendingMessages.append(s)
                }
            }
            return true
        }

        /// Create a JS Promise whose executor stashes/uses the resolver. Runs the
        /// executor synchronously on the JS queue (JSValue promise API).
        private func makePromise(_ job: Job, _ body: @escaping (JSValue, JSValue) -> Void)
            -> JSValue?
        {
            JSValue(newPromiseIn: job.context) { resolve, reject in
                guard let resolve, let reject else { return }
                body(resolve, reject)
            }
        }

        // MARK: - reflect

        private func reflectReply(_ agentId: AgentId, _ kernel: Kernel) -> JSON {
            .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "General-purpose JavaScript executor — runs an authored script as a living agent (kernel connector is its only capability)."
                ),
                "running": .bool(running(agentId)),
                "has_code": .bool(metaCode(agentId, kernel) != nil),
                "verbs": .object([
                    "reflect": .string("Identity + running + has_code. No args."),
                    "boot": .string(
                        "If the record has `code` meta and no script is running, start it (rehydration). Returns {started:true} or null."
                    ),
                    "start": .string(
                        "args: code:str?. (Re)start the script. Inline `code` is persisted onto the record (durable across reboots). Returns {started, running}."
                    ),
                    "stop": .string("No args. Abort the running script. Returns {stopped:bool}."),
                    "status": .string("No args. Returns {running, has_code}."),
                    "shutdown": .string("Abort + drop process state. Returns null."),
                    "<anything else>": .string(
                        "Queued to the SCRIPT's inbox, consumed by await kernel.on_message() — addressable like any agent; reply is {queued:true}. Real output rides emits."
                    ),
                ]),
                "script_api": .object([
                    "await kernel.send(target, payload)": .string(
                        "Dispatch to any agent by id; resolves with the reply object."),
                    "await kernel.emit(payload)": .string(
                        "Emit an event on this agent (watchers + tagged inboxes receive it)."),
                    "await kernel.on_message()": .string(
                        "Resolves with the next queued inbox payload (null when the agent is stopped/deleted)."
                    ),
                    "await kernel.reflect(target?)": .string(
                        "Reflect any agent by id (default 'kernel'); resolves with its reflect reply. Read a bundle's own reflect/readme before authoring `code` against it."
                    ),
                    "kernel.self_id": .string("This agent's id (string)."),
                ]),
                "emits": .object([
                    "job_done": .string(
                        "{type:'job_done', source, ok:bool, error?} — the script ran to completion (or crashed). A harness-style script normally never completes."
                    )
                ]),
                "authoring": .string(
                    "This bundle is how you author DURABLE tools: create_agent handler_module=js_runtime.tools id=<name> code=<js source> — the record's `code` IS the tool, it persists and `boot` rehydrates it after reboot; delete_agent removes it. A running script can itself kernel.send('kernel', {type:'create_agent', ...}) MORE script agents — tools compose into tools."
                ),
            ])
        }
    }

#endif  // canImport(JavaScriptCore)
