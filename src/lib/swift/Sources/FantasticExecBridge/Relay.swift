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
    /// byte. A kind-B `send` rides the batch item's binary channel, a kind-T one the
    /// text channel — the same split a WS transport makes.
    ///
    /// FRAMES ARE SERVED IN ORDER, one at a time, as python's read loop serves
    /// them: a `send` settles (and its reply is pushed) before the next frame is
    /// handled, and consecutive emits fan out in the order they were written.
    ///
    /// | child → parent | parent → child |
    /// |---|---|
    /// | `send{rid,target,payload}` | `reply{rid,data}` |
    /// | `emit{payload}` | — |
    /// | `watch{src}` / `unwatch{src}` | `event{src,payload}` |
    /// | `onmessage` / `offmessage` | `inbox{payload}` |
    final class Relay: @unchecked Sendable {
        private let fd: Int32
        private let owner: AgentId
        /// The job this relay serves — its kernel task id. Proxy ids and the
        /// queue label spell it in decimal, as python's f-strings do.
        private let jobId: TaskId
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
        private var finished = false
        /// The read source's cancel handler has run — only then may the fd close.
        private var sourceDone = false
        /// The frames read off the socket, in wire order, for the ONE consumer
        /// that serves them (python's read loop). Finished at EOF or on `stop`;
        /// a frame already read is still served.
        private let frames: AsyncStream<(JSON, Data?)>
        private let framesIn: AsyncStream<(JSON, Data?)>.Continuation
        /// That consumer, so `stop` can cancel it mid-send the way python's
        /// teardown cancels the relay task.
        private var worker: Task<Void, Never>?

        /// ONE frame bound on this wire, both ends, every runtime. A corrupt length
        /// must never become a multi-GB allocation, and a program that means to move
        /// 20 MB means to STREAM it. Over-limit is a WIRE VIOLATION — the framing
        /// itself is unusable, so there is nothing to reply to and the socket closes;
        /// the connector refuses to write such a frame in the first place.
        private static let maxFrame = 16 * 1024 * 1024

        /// The verbs that DRIVE one job — python `DRIVE_VERBS`. The owner-inbox
        /// relay drops one of these whose `job_id` names ANOTHER job (audit BV-9);
        /// one with no `job_id` (bulk) or naming this job passes. The fanout mirrors
        /// every payload sent to the leg,
        /// so a program started by one caller saw another caller's keystrokes,
        /// paste bytes and drive calls aimed at that caller's job.
        static let driveVerbs: Set<String> = [
            "write", "resize", "paste_image", "stop", "interrupt", "clear",
        ]

        /// Exactly ONE job per owner receives the owner's inbox — a restarted durable
        /// tool whose predecessor lingers would otherwise handle every send twice.
        /// Guarded by its own lock, which is the "external synchronization mechanism"
        /// swift's concurrency checker asks about — every access below goes through it.
        ///
        /// KEYED BY KERNEL, then owner (audit ARCH2-5): swift hosts several kernels
        /// in one process (every test does), and the entry's task and proxy ids are
        /// that kernel's own. Keyed by the bare owner id, kernel B's `onmessage` took
        /// kernel A's entry for the same id and cancelled B's task that happened to
        /// carry A's number, orphaning A's drain. `Kernel.serial`, as the AI side
        /// tables key theirs; a kernel's sub-table goes with its last entry.
        private final class OwnerInboxTable: @unchecked Sendable {
            typealias Entry = (jobId: TaskId, proxy: AgentId, task: TaskId)
            private let lock = NSLock()
            private var map: [UInt64: [String: Entry]] = [:]
            func take(_ kernel: Kernel, _ owner: String) -> Entry? {
                lock.withLock { remove(kernel.serial, owner) }
            }
            func takeIfOwned(_ kernel: Kernel, _ owner: String, by jobId: TaskId) -> Entry? {
                lock.withLock {
                    guard map[kernel.serial]?[owner]?.jobId == jobId else { return nil }
                    return remove(kernel.serial, owner)
                }
            }
            func put(_ kernel: Kernel, _ owner: String, _ e: Entry) {
                lock.withLock { map[kernel.serial, default: [:]][owner] = e }
            }
            /// Under `lock`.
            private func remove(_ serial: UInt64, _ owner: String) -> Entry? {
                let e = map[serial]?.removeValue(forKey: owner)
                if map[serial]?.isEmpty == true { map.removeValue(forKey: serial) }
                return e
            }
        }
        private static let ownerInbox = OwnerInboxTable()

        init(fd: Int32, owner: AgentId, jobId: TaskId, kernel: Kernel) {
            self.fd = fd
            self.owner = owner
            self.jobId = jobId
            self.kernel = kernel
            self.queue = DispatchQueue(label: "exec_bridge.relay.\(jobId)")
            (frames, framesIn) = AsyncStream.makeStream(of: (JSON, Data?).self)
        }

        /// THE RELAY'S KERNEL TASK BODY (py `_serve_ctrl` under `spawn_task(id,
        /// "relay", …)`): open the socket and stay suspended until the child closes
        /// it. Everything the program asks for is forwarded under `taskId`, so a
        /// program's `send` rows under the relay, not under the `start` dispatch that
        /// settled the instant `start` replied.
        func run(taskId: TaskId?) async {
            let started: Bool = await withCheckedContinuation { c in
                queue.async { [weak self] in
                    guard let self, !self.finished else {
                        c.resume(returning: false)
                        return
                    }
                    self.taskId = taskId
                    let src = DispatchSource.makeReadSource(fileDescriptor: self.fd, queue: self.queue)
                    src.setEventHandler { [weak self] in self?.readable() }
                    // EOF or `stop`: no more frames. The ones already read are
                    // still served, then the consumer finishes the relay. The fd
                    // outlives the source: it is closed once both are done.
                    src.setCancelHandler { [weak self] in
                        guard let self else { return }
                        self.sourceDone = true
                        self.framesIn.finish()
                        if self.closed { close(self.fd) }
                    }
                    self.source = src
                    src.resume()
                    c.resume(returning: true)
                }
            }
            guard started else { return }
            // THE ONE CONSUMER — python's read loop: each frame is served to the
            // end before the next one starts.
            let w = Task { [self] in
                for await (m, blob) in frames { await serve(m, blob, relayTask: taskId) }
            }
            await onQueue { self.worker = w }
            await withTaskCancellationHandler {
                await w.value
            } onCancel: {
                w.cancel()
            }
            await onQueue { self.finish() }
        }

        /// Close the socket and stop serving — python's teardown cancels the
        /// relay task, which ends an in-flight send with it. The child's reader
        /// then EOFs and it reaps itself.
        func stop() {
            queue.async { [weak self] in
                guard let self else { return }
                self.worker?.cancel()
                // A stop that beats `run` (a job torn down inside its own `start`)
                // has no source to cancel, so it must finish the relay itself.
                if let src = self.source { src.cancel() } else { self.finish() }
            }
        }

        /// Run `body` on the relay's queue, which owns the socket and the
        /// subscription tables, and wait for it.
        private func onQueue(_ body: @escaping @Sendable () -> Void) async {
            await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                queue.async {
                    body()
                    c.resume()
                }
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
                if let f = Self.unframe(body) { framesIn.yield((f.env, f.body)) }
                // A malformed body skips the frame, never the connection.
            }
        }

        /// Decode one child frame → (envelope, raw body). Kind `T`: JSON, no body.
        /// Kind `B`: the io_bridge binary frame — the envelope keeps `_binary_path`
        /// (its steps; null at that slot) and the body rides alongside, since `JSON`
        /// cannot hold bytes. `nil` for a malformed frame — a `_binary_path` python's
        /// `set_path` cannot place included — or an unknown kind.
        private static func unframe(_ body: Data) -> (env: JSON, body: Data?)? {
            guard let kind = body.first else { return nil }
            let rest = Data(body.dropFirst())
            switch kind {
            case UInt8(ascii: "T"):
                guard let v = try? JSON.parseStrict(rest) else { return nil }
                return (v, nil)
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

        /// Frame one queued inbox item into `envelope` (whose `payload` already
        /// holds the item's head) — kind `T` for plain JSON, kind `B` when the
        /// payload carried raw bytes, `_binary_path` naming where they belong
        /// under `payload`. THE ONE CODEC: a `write_stream` mirrored to a
        /// watching program reaches it whole, not as a header with a hole where
        /// the body was.
        private func pushItem(_ envelope: JSON, _ item: InboxItem) {
            guard let body = item.body else {
                push(envelope)
                return
            }
            var head = envelope
            if case .object(var o) = head {
                o["_binary_path"] = Codec.payloadPath(body.parts)
                head = .object(o)
            }
            pushBinary(header: head, body: body.bytes)
        }

        /// Write one frame: `[4-byte BE len | kind | rest]`.
        private func writeFrame(kind: UInt8, _ rest: Data) {
            // A closed relay has no socket: its fd number may already name
            // something else.
            if closed { return }
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

        /// Serve ONE frame to the end. `send` and `emit` run here, on the
        /// consumer; the subscription ops touch the queue-owned tables, so they
        /// run on the queue — and this waits for them too.
        private func serve(_ m: JSON, _ blob: Data?, relayTask: TaskId?) async {
            guard let op = m["op"].asString, let kernel else { return }
            // THE PROGRAM'S SENDS ARE THIS AGENT'S SENDS, UNDER THE RELAY'S TASK.
            // The relay runs off a DispatchQueue, outside the handler's
            // task-locals, so a forwarded send used to carry NO sender — the
            // OPERATOR, ungated (measured live 2026-09-01: a spawned program
            // reached any agent) — and no parent row. Python's relay is a
            // registered task and inherits the owner's identity; re-enter both.
            let owner = self.owner
            switch op {
            case "send":
                let target = m["target"].asString ?? ""
                // python `m.get("payload") or {}` — a falsy payload IS the empty
                // object, so a program that sends nothing reaches a handler that can
                // read `type` without raising.
                var item = Item(
                    AgentId(target), pyTruthy(m["payload"]) ? m["payload"] : .object([:]))
                // The program's SECRET marks ride onto the item as sent; the
                // kernel validates them (a bad mark is a refusal in the reply)
                // and masks every record of the call. Python `if "secret" in m:
                // item["secret"] = m["secret"]`.
                if let marks = m.asObject?["secret"] { item.secret = marks }
                // A kind-B frame (bytes alongside, `_binary_path` = `["payload", …]`)
                // carries its body on the item's BINARY channel, AT ITS PATH in the
                // payload — `JSON` cannot hold bytes, so that is the only way a
                // program's `write_stream` chunk reaches the bundle, or a
                // `read_stream` chunk comes back.
                // A kind-T frame is JSON and nothing more: a `_binary_path` in it
                // is an inert key (python's `decode_frame` only `loads` a str).
                if let blob {
                    let (p, binary) = Codec.callPayload(m, body: blob)
                    item.payload = pyTruthy(p) ? p : .object([:])
                    item.binary = binary
                }
                // ANY verb's reply may hold raw bytes, whatever frame asked for
                // it (python's relay `encode_frame`s the reply dict; no list of
                // verbs): a call with no request body still carries its reply's
                // body back, and the reply below goes out kind B exactly then.
                if item.binary == nil { item.rawReply = true }
                let sent = item
                let out = await KernelTaskLocals.$currentTask.withValue(relayTask) {
                    await KernelTaskLocals.$currentSender.withValue(owner) {
                        await kernel.send([sent])[0]
                    }
                }
                let rid = m["rid"]
                await onQueue { [weak self] in
                    let env: JSON = .object([
                        "op": .string("reply"), "rid": rid, "data": out.result,
                    ])
                    // THE CARRIER decides the frame kind, never the body's
                    // length: a 0-byte chunk is still a chunk (C37). The chunk
                    // rides at `["data", …its slot]` — `null` there in python's
                    // position — so it lands in the program where the reply holds
                    // it (`r['bytes']` for a `read_stream`, `r['x.y']` for a
                    // reply key `"x.y"`).
                    guard let body = out.body else {
                        self?.push(env)
                        return
                    }
                    let at = body.nested(in: "data")
                    self?.pushBinary(header: Codec.binaryHead(env, at), body: at.bytes)
                }
            case "emit":
                // A program emits on its OWNER agent — the one it runs as (python
                // `kernel.emit(owner_id, m.get("payload") or {})`, `m` DECODED). A
                // kind-B frame's bytes sit IN that payload at their `_binary_path`
                // (`["payload", …steps]`), so the emit carries them there — the
                // inbox item holds them beside the JSON, `null` in their slot — and
                // a watcher receives the payload WHOLE. A body the frame put
                // outside `payload` is not part of what python emits.
                let payload = pyTruthy(m["payload"]) ? m["payload"] : .object([:])
                let item: InboxItem =
                    blob.flatMap { Codec.payloadBody(m, body: $0) }.map { .binary(payload, $0) }
                    ?? .text(payload)
                KernelTaskLocals.$currentTask.withValue(relayTask) {
                    KernelTaskLocals.$currentSender.withValue(owner) {
                        kernel.emitItem(owner, item)
                    }
                }
            default:
                await onQueue { [weak self] in self?.subscription(op, m, kernel) }
            }
        }

        /// The subscription ops — `watch`/`unwatch`/`onmessage`/`offmessage`.
        /// Queue-confined: they own `subs` and `selfProxy`.
        private func subscription(_ op: String, _ m: JSON, _ kernel: Kernel) {
            switch op {
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
                                self?.pushItem(
                                    .object([
                                        "op": .string("event"), "src": .string(src),
                                        "payload": p.payload,
                                    ]), p)
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
                let job = jobId.value
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
                    kernel.spawnTask(agent: o, verb: "inbox_drain") { [weak self, weak kernel] in
                        for await p in stream {
                            // The job's own telemetry echoes back through the owner's
                            // fanout; drop it so a tool does not receive its own
                            // progress/job_started/job_done as inbox mail.
                            let verb = p.payload["type"].asString
                            switch verb {
                            case "progress", "output", "job_started", "job_done": continue
                            default: break
                            }
                            // …a drive verb addressed to ANOTHER job (audit BV-9) —
                            // an int naming a job other than THIS one; one with no
                            // `job_id` (bulk) or naming this job is forwarded…
                            if let verb, Self.driveVerbs.contains(verb),
                                case .integer(let n) = p.payload["job_id"], n != job
                            {
                                continue
                            }
                            // …and whatever the leg's gate refuses (python
                            // `_inbox_framer`, audit BV-8): the fanout mirrors every
                            // payload here BEFORE any handler runs, so a sealed leg
                            // whose handler refused the call still had it delivered.
                            // An unknown or malformed rule resolves to
                            // `DenyInbound(unknown:)`, which refuses every payload.
                            guard let kernel else { return }
                            if ExecBridgeBundle.gate(o, verb: verb ?? "", kernel: kernel) != nil {
                                continue
                            }
                            self?.queue.async {
                                self?.pushItem(
                                    .object(["op": .string("inbox"), "payload": p.payload]), p)
                            }
                        }
                    }
                }
                selfProxy = proxy
                Self.ownerInbox.put(kernel, owner.value, (jobId, proxy, t))
            case "offmessage":
                selfProxy = nil
                releaseOwnerInbox(kernel)
            default:
                break
            }
        }

        /// Unhook whichever job currently holds the owner's inbox IN THIS KERNEL,
        /// whoever owns it. The handle lives in the static table precisely so a
        /// supersede can reach a predecessor served by a DIFFERENT relay.
        private func supersedePredecessor(_ kernel: Kernel) {
            guard let prev = Self.ownerInbox.take(kernel, owner.value) else { return }
            let o = owner
            Task { _ = await kernel.cancelTask(prev.task) }
            Task { await kernel.unwatch(src: o, watcher: prev.proxy) }
            kernel.removeInbox(prev.proxy)
        }

        /// Give up the owner's inbox — but ONLY if it still names this job. A superseded
        /// predecessor tearing down late must not unhook its replacement.
        private func releaseOwnerInbox(_ kernel: Kernel) {
            guard let mine = Self.ownerInbox.takeIfOwned(kernel, owner.value, by: jobId) else {
                return
            }
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
            framesIn.finish()
            // A live read source must be cancelled before its fd closes; its
            // cancel handler closes it then.
            if let src = source, !sourceDone {
                src.cancel()
            } else {
                close(fd)
            }
            finished = true
        }
    }
#endif
