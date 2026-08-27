#if os(macOS)
    import FantasticJSON
    import FantasticKernel
    import Foundation

    /// The PARENT half of the child's control socket — the swift port of python's
    /// `_serve_ctrl`, op for op.
    ///
    /// Wire: `[4-byte BE length | JSON body]`, both directions. Deliberately NOT
    /// `FantasticIoBridge`'s codec, which is a THREE-part binary frame
    /// (`[len | JSON header | raw body]`) for carrying bytes without base64. They share
    /// only the length prefix; wiring them together would be a silent interop bug.
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
        private var subs: [String: (proxy: AgentId, task: Task<Void, Never>)] = [:]
        private var selfProxy: AgentId?
        private var closed = false

        /// A corrupt length must never become a multi-GB allocation.
        private static let maxFrame = 16 * 1024 * 1024

        /// Exactly ONE job per owner receives the owner's inbox — a restarted durable
        /// tool whose predecessor lingers would otherwise handle every send twice.
        /// Guarded by its own lock, which is the "external synchronization mechanism"
        /// swift's concurrency checker asks about — every access below goes through it.
        private final class OwnerInboxTable: @unchecked Sendable {
            private let lock = NSLock()
            private var map: [String: (jobId: String, proxy: AgentId, task: Task<Void, Never>)] = [:]
            func take(_ owner: String) -> (jobId: String, proxy: AgentId, task: Task<Void, Never>)? {
                lock.withLock { map.removeValue(forKey: owner) }
            }
            func takeIfOwned(_ owner: String, by jobId: String)
                -> (jobId: String, proxy: AgentId, task: Task<Void, Never>)?
            {
                lock.withLock {
                    guard map[owner]?.jobId == jobId else { return nil }
                    return map.removeValue(forKey: owner)
                }
            }
            func put(_ owner: String, _ e: (jobId: String, proxy: AgentId, task: Task<Void, Never>)) {
                lock.withLock { map[owner] = e }
            }
        }
        private static let ownerInbox = OwnerInboxTable()

        init(fd: Int32, owner: AgentId, jobId: String, kernel: Kernel) {
            self.fd = fd
            self.owner = owner
            self.jobId = jobId
            self.kernel = kernel
            self.queue = DispatchQueue(label: "py.relay.\(jobId)")
        }

        func start() {
            let src = DispatchSource.makeReadSource(fileDescriptor: fd, queue: queue)
            src.setEventHandler { [weak self] in self?.readable() }
            src.setCancelHandler { [weak self] in self?.finish() }
            source = src
            src.resume()
        }

        /// Close the socket. The child's reader then EOFs and it reaps itself.
        func stop() {
            queue.async { [weak self] in self?.source?.cancel() }
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
                if let v = try? JSON.parse(body) { handle(v) }
                // A malformed body skips the frame, never the connection.
            }
        }

        private func push(_ v: JSON) {
            let body = Data(v.serialize().utf8)
            var frame = Data()
            let n = UInt32(body.count)
            frame.append(contentsOf: [
                UInt8((n >> 24) & 0xFF), UInt8((n >> 16) & 0xFF),
                UInt8((n >> 8) & 0xFF), UInt8(n & 0xFF),
            ])
            frame.append(body)
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

        private func handle(_ m: JSON) {
            guard let op = m["op"].asString, let kernel else { return }
            switch op {
            case "send":
                let target = m["target"].asString ?? ""
                let payload = m["payload"]
                let rid = m["rid"]
                Task { [weak self] in
                    let data = await kernel.send([Item(AgentId(target), payload)])[0].result
                    self?.queue.async {
                        self?.push(.object(["op": .string("reply"), "rid": rid, "data": data]))
                    }
                }
            case "emit":
                // No `target` ⇒ the OWNER agent (the one-arg `emit(payload)` form).
                let tgt = m["target"].asString.map { AgentId($0) } ?? owner
                let payload = m["payload"]
                Task { await kernel.emit(tgt, payload) }
            case "watch":
                guard let src = m["src"].asString, subs[src] == nil else { return }
                let proxy = AgentId("_pyw_\(jobId)_\(src)")
                let stream = kernel.ensureInbox(proxy)
                let srcId = AgentId(src)
                Task { await kernel.watch(src: srcId, watcher: proxy) }
                let t = Task { [weak self] in
                    for await p in stream {
                        self?.queue.async {
                            self?.push(
                                .object([
                                    "op": .string("event"), "src": .string(src), "payload": p,
                                ]))
                        }
                    }
                }
                subs[src] = (proxy, t)
            case "unwatch":
                guard let src = m["src"].asString, let sub = subs.removeValue(forKey: src)
                else { return }
                sub.task.cancel()
                Task { await kernel.unwatch(src: AgentId(src), watcher: sub.proxy) }
            case "onmessage":
                guard selfProxy == nil else { return }  // a second onmessage is a no-op
                supersedePredecessor(kernel)
                let proxy = AgentId("_pyself_\(jobId)")
                let stream = kernel.ensureInbox(proxy)
                let o = owner
                Task { await kernel.watch(src: o, watcher: proxy) }
                let t = Task { [weak self] in
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
            prev.task.cancel()
            let o = owner
            Task { await kernel.unwatch(src: o, watcher: prev.proxy) }
        }

        /// Give up the owner's inbox — but ONLY if it still names this job. A superseded
        /// predecessor tearing down late must not unhook its replacement.
        private func releaseOwnerInbox(_ kernel: Kernel) {
            guard let mine = Self.ownerInbox.takeIfOwned(owner.value, by: jobId) else { return }
            mine.task.cancel()
            let o = owner
            Task { await kernel.unwatch(src: o, watcher: mine.proxy) }
        }

        // MARK: - teardown

        private func finish() {
            guard !closed else { return }
            closed = true
            if let kernel {
                for (src, sub) in subs {
                    sub.task.cancel()
                    Task { await kernel.unwatch(src: AgentId(src), watcher: sub.proxy) }
                }
                if selfProxy != nil { releaseOwnerInbox(kernel) }
            }
            subs.removeAll()
            selfProxy = nil
            close(fd)
        }
    }
#endif
