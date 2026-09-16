#if os(macOS)
    import FantasticIoBridge
    import FantasticJSON
    import FantasticKernel
    import Foundation

    /// The PARENT half of the child's control socket — the swift port of python's
    /// `_serve_ctrl`, op for op.
    ///
    /// Wire: `[4-byte BE length | kind | rest]`, both directions. Kind `T`: a JSON
    /// envelope. Kind `B`: `FantasticIoBridge.Codec`'s binary frame
    /// (`[len | JSON header | raw body]`, the header's `_binary_path` naming where the
    /// body belongs) — how a `read_stream` chunk or a `write_stream` payload crosses
    /// the fd raw, never base64. A raw fd has no WS text/binary frame kind, hence the
    /// byte. A kind-B `send` rides the kernel's binary channel (`sendWithBinary`), a
    /// kind-T one the text channel — the same split a WS transport makes.
    ///
    /// | child → parent | parent → child |
    /// |---|---|
    /// | `send{rid,target,payload}` | `reply{rid,data}` |
    /// | `emit{target?,payload}` | — |
    /// | `watch{src}` / `unwatch{src}` | `event{src,payload}` |
    /// | `onmessage` / `offmessage` | `inbox{payload}` |
    final class Relay: @unchecked Sendable {
        private let fd: Int32
        private let owner: AgentId
        private let jobId: String
        private weak var kernel: Kernel?
        /// Owns the socket, the read buffer and the subscription tables. Nothing here is
        /// touched off this queue, so none of it needs a lock.
        private let queue: DispatchQueue
        private var source: DispatchSourceRead?
        private var buf = Data()
        private var subs: [String: (proxy: AgentId, task: TaskId)] = [:]
        private var selfProxy: AgentId?
        private var closed = false
        /// This relay's own row in the kernel task table (py `spawn_task(id, "relay",
        /// …)`). Every forwarded send/emit and every drain is parented to it, so a
        /// program's work is rowed under work that is still alive.
        private var taskId: TaskId?
        /// Resumed once, when the socket closes — what keeps the relay's kernel task
        /// alive for exactly as long as the child's wire is.
        private var done: CheckedContinuation<Void, Never>?
        private var finished = false

        /// ONE frame bound on this wire, both ends, every runtime. A corrupt length
        /// must never become a multi-GB allocation, and a program that means to move
        /// 20 MB means to STREAM it. Over-limit is a WIRE VIOLATION — the framing
        /// itself is unusable, so there is nothing to reply to and the socket closes;
        /// the connector refuses to write such a frame in the first place.
        private static let maxFrame = 16 * 1024 * 1024

        /// Exactly ONE job per owner receives the owner's inbox — a restarted durable
        /// tool whose predecessor lingers would otherwise handle every send twice.
        /// Guarded by its own lock, which is the "external synchronization mechanism"
        /// swift's concurrency checker asks about — every access below goes through it.
        private final class OwnerInboxTable: @unchecked Sendable {
            private let lock = NSLock()
            private var map: [String: (jobId: String, proxy: AgentId, task: TaskId)] = [:]
            func take(_ owner: String) -> (jobId: String, proxy: AgentId, task: TaskId)? {
                lock.withLock { map.removeValue(forKey: owner) }
            }
            func takeIfOwned(_ owner: String, by jobId: String)
                -> (jobId: String, proxy: AgentId, task: TaskId)?
            {
                lock.withLock {
                    guard map[owner]?.jobId == jobId else { return nil }
                    return map.removeValue(forKey: owner)
                }
            }
            func put(_ owner: String, _ e: (jobId: String, proxy: AgentId, task: TaskId)) {
                lock.withLock { map[owner] = e }
            }
        }
        private static let ownerInbox = OwnerInboxTable()

        init(fd: Int32, owner: AgentId, jobId: String, kernel: Kernel) {
            self.fd = fd
            self.owner = owner
            self.jobId = jobId
            self.kernel = kernel
            self.queue = DispatchQueue(label: "exec_bridge.relay.\(jobId)")
        }

        /// THE RELAY'S KERNEL TASK BODY (py `_serve_ctrl` under `spawn_task(id,
        /// "relay", …)`): open the socket and stay suspended until the child closes
        /// it. Everything the program asks for is forwarded under `taskId`, so a
        /// program's `send` rows under the relay, not under the `start` dispatch that
        /// settled the instant `start` replied.
        func run(taskId: TaskId?) async {
            await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                queue.async { [weak self] in
                    guard let self else {
                        c.resume()
                        return
                    }
                    if self.finished {
                        c.resume()
                        return
                    }
                    self.taskId = taskId
                    self.done = c
                    let src = DispatchSource.makeReadSource(fileDescriptor: self.fd, queue: self.queue)
                    src.setEventHandler { [weak self] in self?.readable() }
                    src.setCancelHandler { [weak self] in self?.finish() }
                    self.source = src
                    src.resume()
                }
            }
        }

        /// Close the socket. The child's reader then EOFs and it reaps itself.
        func stop() {
            queue.async { [weak self] in
                guard let self else { return }
                // A stop that beats `run` (a job torn down inside its own `start`)
                // has no source to cancel, so it must finish the relay itself.
                if let src = self.source { src.cancel() } else { self.finish() }
            }
        }

        // MARK: - reading

        private func readable() {
            var chunk = [UInt8](repeating: 0, count: 65536)
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 {  // EOF or error — the child is gone
                source?.cancel()
                return
            }
            buf.append(contentsOf: chunk[0..<n])
            while true {
                guard buf.count >= 4 else { return }
                let len = buf.prefix(4).reduce(0) { Int($0) << 8 | Int($1) }
                if len > Self.maxFrame {
                    source?.cancel()
                    return
                }
                guard buf.count >= 4 + len else { return }
                let body = buf.subdata(in: 4..<(4 + len))
                buf.removeSubrange(0..<(4 + len))
                if let f = Self.unframe(body) { handle(f.env, f.body) }
                // A malformed body skips the frame, never the connection.
            }
        }

        /// Decode one child frame → (envelope, raw body). Kind `T`: JSON, empty body.
        /// Kind `B`: the io_bridge binary frame — the envelope keeps `_binary_path`
        /// (null there) and the body rides alongside, since `JSON` cannot hold bytes.
        /// `nil` for a malformed frame or an unknown kind.
        private static func unframe(_ body: Data) -> (env: JSON, body: Data)? {
            guard let kind = body.first else { return nil }
            let rest = Data(body.dropFirst())
            switch kind {
            case UInt8(ascii: "T"):
                guard let v = try? JSON.parse(rest) else { return nil }
                return (v, Data())
            case UInt8(ascii: "B"):
                guard let f = try? Codec.decodeBinaryFrame(rest) else { return nil }
                return (f.header, f.body)
            default:
                return nil
            }
        }

        /// Frame one JSON value onto the socket (kind `T`).
        private func push(_ v: JSON) {
            writeFrame(kind: UInt8(ascii: "T"), Data(Codec.encodeText(v).utf8))
        }

        /// Frame a header + raw body onto the socket (kind `B`). `header` already
        /// carries `_binary_path` with null at that spot.
        private func pushBinary(header: JSON, body: Data) {
            writeFrame(kind: UInt8(ascii: "B"), Codec.encodeBinaryFrame(header: header, body: body))
        }

        /// Write one frame: `[4-byte BE len | kind | rest]`.
        private func writeFrame(kind: UInt8, _ rest: Data) {
            var frame = Data()
            let n = UInt32(rest.count + 1)
            frame.append(contentsOf: [
                UInt8((n >> 24) & 0xFF), UInt8((n >> 16) & 0xFF),
                UInt8((n >> 8) & 0xFF), UInt8(n & 0xFF),
            ])
            frame.append(kind)
            frame.append(rest)
            frame.withUnsafeBytes { raw in
                var off = 0
                while off < raw.count {
                    let w = write(fd, raw.baseAddress!.advanced(by: off), raw.count - off)
                    if w <= 0 { return }
                    off += w
                }
            }
        }

        // MARK: - ops

        private func handle(_ m: JSON, _ blob: Data) {
            guard let op = m["op"].asString, let kernel else { return }
            switch op {
            case "send":
                let target = m["target"].asString ?? ""
                // python `m.get("payload") or {}` — a falsy payload IS the empty
                // object, so a program that sends nothing reaches a handler that can
                // read `type` without raising.
                let payload = pyTruthy(m["payload"]) ? m["payload"] : .object([:])
                let rid = m["rid"]
                // A kind-B frame (bytes alongside, `_binary_path` = `payload.<field>`)
                // is the BINARY channel — `sendWithBinary`, as a WS binary frame would
                // be. Kind T is the text channel. `JSON` cannot carry bytes, so this
                // split is the only way a program's `write_stream` payload reaches the
                // bundle, or a `read_stream` chunk comes back.
                let binary = m["_binary_path"] != .null
                let relayTask = taskId
                Task { [weak self] in
                    // THE PROGRAM'S SENDS ARE THIS AGENT'S SENDS, UNDER THE RELAY'S
                    // TASK. The relay runs off a DispatchQueue, outside the handler's
                    // task-locals, so this forwarded send used to carry NO sender —
                    // the OPERATOR, ungated (measured live 2026-09-01: a spawned
                    // program reached any agent) — and no parent row. Python's relay
                    // is a registered task and inherits the owner's identity; re-enter
                    // both here.
                    let owner = self?.owner
                    let (data, body): (JSON, Data) = await KernelTaskLocals.$currentTask
                        .withValue(relayTask) {
                            await KernelTaskLocals.$currentSender.withValue(owner) {
                                if binary {
                                    return await kernel.sendWithBinary(
                                        AgentId(target), payload, blob)
                                }
                                return (
                                    await kernel.send([Item(AgentId(target), payload)])[0].result,
                                    Data()
                                )
                            }
                        }
                    self?.queue.async {
                        var env: JSON = .object(["op": .string("reply"), "rid": rid, "data": data])
                        if body.isEmpty {
                            self?.push(env)
                        } else {
                            // A `read_stream` chunk: the reply body rides raw at
                            // `data.bytes` (python's reply dict holds it there natively).
                            env["data"]["bytes"] = .null
                            env["_binary_path"] = .string("data.bytes")
                            self?.pushBinary(header: env, body: body)
                        }
                    }
                }
            case "emit":
                // python `m.get("target") or owner_id` / `m.get("payload") or {}` —
                // a FALSY target (absent, null, `""`) is the OWNER agent, which is the
                // one-arg `emit(payload)` form.
                let raw = m["target"]
                let tgt = pyTruthy(raw) ? AgentId(pyStr(raw)) : owner
                let payload = pyTruthy(m["payload"]) ? m["payload"] : .object([:])
                let relayTask = taskId
                Task {
                    await KernelTaskLocals.$currentTask.withValue(relayTask) {
                        await kernel.emit(tgt, payload)
                    }
                }
            case "watch":
                guard let src = m["src"].asString, subs[src] == nil else { return }
                // `_xb…`, not `_pyw…`: two spawner bundles in one kernel must not
                // collide on a synthetic inbox id, and this one runs COMMANDS — the
                // name has claimed "python" since before the runner was general.
                let proxy = AgentId("_xbw_\(jobId)_\(src)")
                let srcId = AgentId(src)
                // THE PROGRAM'S WATCH IS THIS AGENT'S WATCH, and THE KERNEL DECIDES.
                // Off the handler's task-local this relay watched as the OPERATOR —
                // ungated, so a refusal was not merely discarded, it could never
                // happen. Re-enter the owner's identity (as the `send` case does)
                // and register NOTHING when the answer is no.
                //
                // THE READER REGISTERS FIRST: `watch` refuses a watcher with no
                // inbox, and the drain task below is what reads this one — so
                // claim the stream before asking to be watched, and drop it
                // again if the answer is no.
                let stream = kernel.ensureInbox(proxy)
                let denial = KernelTaskLocals.$currentSender.withValue(owner) {
                    kernel.watch(src: srcId, watcher: proxy)
                }
                if let denial {
                    kernel.removeInbox(proxy)
                    push(
                        .object([
                            "op": .string("event"), "src": .string(src), "payload": denial,
                        ]))
                    return
                }
                // A DRAIN LIVES AS LONG AS THE SUBSCRIPTION, and it is a kernel TASK
                // (py `spawn_task(id, "watch_drain", …)`): `ps` shows what a watching
                // program is holding, and it is parented to this relay.
                let t = KernelTaskLocals.$currentTask.withValue(taskId) {
                    kernel.spawnTask(agent: owner, verb: "watch_drain") { [weak self] in
                        for await p in stream {
                            self?.queue.async {
                                self?.push(
                                    .object([
                                        "op": .string("event"), "src": .string(src),
                                        "payload": p,
                                    ]))
                            }
                        }
                    }
                }
                subs[src] = (proxy, t)
            case "unwatch":
                guard let src = m["src"].asString, let sub = subs.removeValue(forKey: src)
                else { return }
                Task { _ = await kernel.cancelTask(sub.task) }
                Task { await kernel.unwatch(src: AgentId(src), watcher: sub.proxy) }
                kernel.removeInbox(sub.proxy)  // the proxy row is OURS to release
            case "onmessage":
                guard selfProxy == nil else { return }  // a second onmessage is a no-op
                supersedePredecessor(kernel)
                let proxy = AgentId("_xbself_\(jobId)")
                let o = owner
                let stream = kernel.ensureInbox(proxy)
                let denial = KernelTaskLocals.$currentSender.withValue(o) {
                    kernel.watch(src: o, watcher: proxy)
                }
                if let denial {
                    kernel.removeInbox(proxy)
                    push(
                        .object([
                            "op": .string("event"), "src": .string(o.value), "payload": denial,
                        ]))
                    return
                }
                let t = KernelTaskLocals.$currentTask.withValue(taskId) {
                    kernel.spawnTask(agent: o, verb: "inbox_drain") { [weak self] in
                        for await p in stream {
                            // The job's own telemetry echoes back through the owner's
                            // fanout; drop it so a tool does not receive its own
                            // progress/job_started/job_done as inbox mail.
                            switch p["type"].asString {
                            case "progress", "job_started", "job_done": continue
                            default: break
                            }
                            self?.queue.async {
                                self?.push(.object(["op": .string("inbox"), "payload": p]))
                            }
                        }
                    }
                }
                selfProxy = proxy
                Self.ownerInbox.put(owner.value, (jobId, proxy, t))
            case "offmessage":
                selfProxy = nil
                releaseOwnerInbox(kernel)
            default:
                break
            }
        }

        /// Unhook whichever job currently holds the owner's inbox, whoever owns it.
        /// The handle lives in the static table precisely so a supersede can reach a
        /// predecessor served by a DIFFERENT relay.
        private func supersedePredecessor(_ kernel: Kernel) {
            guard let prev = Self.ownerInbox.take(owner.value) else { return }
            let o = owner
            Task { _ = await kernel.cancelTask(prev.task) }
            Task { await kernel.unwatch(src: o, watcher: prev.proxy) }
            kernel.removeInbox(prev.proxy)
        }

        /// Give up the owner's inbox — but ONLY if it still names this job. A superseded
        /// predecessor tearing down late must not unhook its replacement.
        private func releaseOwnerInbox(_ kernel: Kernel) {
            guard let mine = Self.ownerInbox.takeIfOwned(owner.value, by: jobId) else { return }
            let o = owner
            Task { _ = await kernel.cancelTask(mine.task) }
            Task { await kernel.unwatch(src: o, watcher: mine.proxy) }
            kernel.removeInbox(mine.proxy)
        }

        // MARK: - teardown

        private func finish() {
            guard !closed else { return }
            closed = true
            if let kernel {
                for (src, sub) in subs {
                    Task { _ = await kernel.cancelTask(sub.task) }
                    Task { await kernel.unwatch(src: AgentId(src), watcher: sub.proxy) }
                    kernel.removeInbox(sub.proxy)  // the proxy row is OURS to release
                }
                if selfProxy != nil { releaseOwnerInbox(kernel) }
            }
            subs.removeAll()
            selfProxy = nil
            close(fd)
            // The relay's kernel task ends with its socket — not before, not after.
            finished = true
            let c = done
            done = nil
            c?.resume()
        }
    }
#endif
