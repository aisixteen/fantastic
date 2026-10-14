// MONITORS — a filter on a running job's OUTPUT that tells an
// agent when something it watches for appears: how a long run is stopped EARLY.
// Python `exec_bridge/_monitor.py`, rule for rule.
//
// A job's output already flows through this bridge line by line, so a monitor POLLS
// NOTHING: each line is matched as it arrives (`match`, a regex in python's `re`
// dialect, searched; absent = every line), and a match is SENT to `notify` as a turn.
// `every` throttles delivery — the first match goes at once, later ones are gathered
// and delivered at most once per `every` seconds — and a timer exists only while
// something waits. An idle monitor wakes never. `tail` caps the lines in one
// delivery to the newest N.
//
// Each delivery is a call made by this bridge AS ITSELF (its `can_call` must name
// `notify`, and `notify`'s `callable_by` this bridge), fire-and-forget as its own
// task. A monitor IS a kernel task (its id is the `monitor_id`): `ps` lists it, kernel
// `cancel` (or `unmonitor`) ends it, and it ends with its job — after delivering what
// still waited. RAM only, like the job it watches.
//
// A DELIVERY THE TARGET REFUSES (the call gate: `unauthorized`; no such agent:
// `unknown_agent`) ENDS the monitor — never a silent drop. The dead monitor stays in
// `monitors` with `status: "dead"` and the refusal as `error` until `unmonitor`
// clears it.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import OrderedCollections

    /// Lines in one delivery when the caller names no `tail`.
    let MONITOR_DEFAULT_TAIL: Int64 = 100

    /// What a refused delivery answers — the target will never take one.
    let MONITOR_REFUSED: Set<String> = ["unauthorized", "unknown_agent"]

    /// What `monitor` was asked for, checked — python `_monitor_verb` past its job checks.
    struct MonitorArgs {
        let notify: String
        let pattern: String?
        let matcher: NSRegularExpression?
        /// As the caller wrote it (an int stays an int), echoed by `monitor`/`monitors`.
        let every: JSON
        let everySecs: Double?
        let tail: Int64
        let label: String?

        /// The arguments after `job_id`, in python's order, or the refusal.
        static func parse(_ payload: JSON, kernel: Kernel) -> Result<MonitorArgs, ExecBridgeBundle.RefusalJSON> {
            func no(_ r: JSON) -> Result<MonitorArgs, ExecBridgeBundle.RefusalJSON> {
                .failure(ExecBridgeBundle.RefusalJSON(json: r))
            }
            let notify: String
            switch payload["notify"] {
            case .null: return no(Refusal.missing("monitor", "notify"))
            case .string(let s) where !s.isEmpty: notify = s
            default: return no(Refusal.bad("monitor", "notify", "an agent id"))
            }
            if kernel.agent(AgentId(notify)) == nil {
                return no(
                    Refusal.make(
                        Refusal.notFound, "monitor: no agent \(pyRepr(.string(notify))) to notify",
                        extra: [("arg", .string("notify"))]))
            }
            var pattern: String? = nil
            var matcher: NSRegularExpression? = nil
            switch payload["match"] {
            case .null: break
            case .string(let s) where !s.isEmpty:
                do {
                    matcher = try NSRegularExpression(pattern: pyRegexSource(s))
                    pattern = s
                } catch {
                    return no(Refusal.bad("monitor", "match", "a valid regex (\(error.localizedDescription))"))
                }
            default: return no(Refusal.bad("monitor", "match", "a regex string"))
            }
            let every = payload["every"]
            var everySecs: Double? = nil
            switch every {
            case .null: break
            case .integer(let n) where n > 0: everySecs = Double(n)
            case .double(let d) where d > 0: everySecs = d
            default: return no(Refusal.bad("monitor", "every", "a positive number of seconds"))
            }
            if matcher == nil, everySecs == nil {
                return no(
                    Refusal.make(
                        Refusal.missingArg,
                        "monitor: every required — without `match` every line counts, so name the "
                            + "window (`every`, seconds) its deliveries are gathered in",
                        extra: [("arg", .string("every"))]))
            }
            // python `payload.get("tail", DEFAULT_TAIL)`: only the KEY absent is the default.
            var tail = MONITOR_DEFAULT_TAIL
            if let raw = payload.asObject?["tail"] {
                guard case .integer(let n) = raw, n >= 1 else {
                    return no(Refusal.bad("monitor", "tail", "an int >= 1 (lines per delivery)"))
                }
                tail = n
            }
            var label: String? = nil
            switch payload["label"] {
            case .null: break
            case .string(let s): label = s.isEmpty ? nil : s
            default: return no(Refusal.bad("monitor", "label", "a string"))
            }
            return .success(
                MonitorArgs(
                    notify: notify, pattern: pattern, matcher: matcher, every: every,
                    everySecs: everySecs, tail: tail, label: label))
        }
    }

    /// One monitor: what it was asked for, and the lines matched but not yet taken by
    /// its task. Every field that changes is guarded by `lock`, never held across an
    /// `await`.
    final class Monitor: @unchecked Sendable {
        let owner: AgentId
        let jobId: TaskId
        let args: MonitorArgs
        /// Who called `monitor`, and that call's id — the delivery header's
        /// "from your call <pid>" (nil = the operator; a call with no task reads None).
        let caller: String?
        let call: Int64?

        private let lock = NSLock()
        /// The monitor's task id, known once its fork exists; 0 until then.
        private var id: Int64 = 0
        private var inbox: [String] = []
        private var endSeen = false
        private var carry = ""
        private var kicked = false
        private var waiter: CheckedContinuation<Void, Never>? = nil
        private var delivered: Int64 = 0
        private var pending: Int64 = 0
        /// Set when `notify` refused a delivery: the monitor is dead.
        private var error: String? = nil

        init(owner: AgentId, jobId: TaskId, args: MonitorArgs, caller: String?, call: Int64?) {
            self.owner = owner
            self.jobId = jobId
            self.args = args
            self.caller = caller
            self.call = call
        }

        var monitorId: Int64 { lock.withLock { id } }

        var isDead: Bool { lock.withLock { error != nil } }

        /// The id is known: from here on what was fed counts.
        func arm(_ taskId: TaskId) {
            lock.withLock { id = taskId.value }
            wake()
        }

        private func matches(_ line: String) -> Bool {
            guard let m = args.matcher else { return true }
            return m.firstMatch(in: line, range: NSRange(line.startIndex..., in: line)) != nil
        }

        /// python `rstrip("\r")`, on scalars (a Swift `Character` folds "\r\n").
        private static func trimCR(_ s: some StringProtocol) -> String {
            var u = Substring(s).unicodeScalars
            while u.last == "\r" { u.removeLast() }
            return String(u)
        }

        private func push(_ lines: [String], end: Bool) {
            let w = lock.withLock { () -> CheckedContinuation<Void, Never>? in
                inbox += lines
                if end { endSeen = true }
                let w = (id != 0 && (!inbox.isEmpty || endSeen)) ? waiter : nil
                if w != nil { waiter = nil }
                return w
            }
            w?.resume()
        }

        /// One output line of the job.
        func feedLine(_ line: String) {
            if matches(line) { push([line], end: false) }
        }

        /// A PTY chunk: split into lines (a partial line waits for its end).
        func feedText(_ text: String) {
            let lines = lock.withLock { () -> [String] in
                var parts = (carry + text).components(separatedBy: "\n")
                carry = parts.removeLast()
                return parts
            }
            let hit = lines.map { Self.trimCR($0) }.filter(matches)
            if !hit.isEmpty { push(hit, end: false) }
        }

        /// The job is gone: what waits is delivered, then the monitor ends.
        func jobEnded() {
            let rest = lock.withLock { () -> String in
                defer { carry = "" }
                return carry
            }
            var last: [String] = []
            if !rest.isEmpty {
                let line = Self.trimCR(rest)
                if matches(line) { last = [line] }
            }
            push(last, end: true)
        }

        private func wake() {
            let w = lock.withLock { () -> CheckedContinuation<Void, Never>? in
                guard let w = waiter else {
                    kicked = true
                    return nil
                }
                waiter = nil
                return w
            }
            w?.resume()
        }

        /// Wait until something was fed (once armed), `timeout` seconds pass, or the task
        /// is cancelled. A timer runs only when `timeout` is set.
        private func wait(timeout: Double?) async {
            let timer = timeout.map { secs in
                Task { [weak self] in
                    try? await Task.sleep(nanoseconds: UInt64(max(0, secs) * 1e9))
                    if !Task.isCancelled { self?.wake() }
                }
            }
            await withTaskCancellationHandler {
                await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                    let ready = lock.withLock { () -> Bool in
                        if kicked || Task.isCancelled || (id != 0 && (!inbox.isEmpty || endSeen)) {
                            kicked = false
                            return true
                        }
                        waiter = c
                        return false
                    }
                    if ready { c.resume() }
                }
            } onCancel: {
                self.wake()
            }
            timer?.cancel()
        }

        private func take() -> (lines: [String], ended: Bool) {
            lock.withLock {
                guard id != 0 else { return ([], false) }
                defer { inbox = [] }
                return (inbox, endSeen)
            }
        }

        private func setPending(_ n: Int) {
            lock.withLock { pending = Int64(n) }
        }

        /// The monitor's task: gather what matched, deliver it on the throttle, end with
        /// the job or a cancel. No timer while nothing waits.
        func run(kernel: Kernel, table: MonitorTable) async {
            // a DEAD monitor stays listed (its `error` says why) until `unmonitor`
            defer { if !isDead { table.remove(self) } }
            let clock = ContinuousClock()
            var last: ContinuousClock.Instant? = nil
            var pending: [String] = []
            var ended = false
            func elapsed() -> Double {
                guard let last else { return .infinity }
                let c = last.duration(to: clock.now).components
                return Double(c.seconds) + Double(c.attoseconds) / 1e18
            }
            func windowOpen() -> Bool {
                guard let e = args.everySecs else { return true }
                return elapsed() >= e
            }
            while !Task.isCancelled {
                if pending.isEmpty || !windowOpen() {
                    // throttled: wait out the rest of the window; idle: wait for a line
                    let timeout = pending.isEmpty ? nil : (args.everySecs ?? 0) - elapsed()
                    await wait(timeout: timeout)
                    if Task.isCancelled || isDead { return }
                    let (got, end) = take()
                    pending += got
                    if end { ended = true }
                    setPending(pending.count)
                }
                if !pending.isEmpty && (ended || windowOpen()) {
                    deliver(pending, ended: ended, kernel: kernel)
                    last = clock.now
                    pending = []
                    setPending(0)
                }
                if ended { return }
            }
        }

        func header(ended: Bool) -> String {
            let who = caller == args.notify ? "your" : "\(caller ?? "the operator")'s"
            var parts = [
                "monitor \(monitorId)", "job \(jobId.value)",
                "from \(who) call \(call.map { String($0) } ?? "None")",
            ]
            if let l = args.label { parts.append("\"\(l)\"") }
            if ended { parts.append("job ended") }
            return "[" + parts.joined(separator: " · ") + "]"
        }

        private func deliver(_ lines: [String], ended: Bool, kernel: Kernel) {
            let keep = Int(min(args.tail, Int64(lines.count)))
            let dropped = lines.count - keep
            var body = lines.suffix(keep).joined(separator: "\n")
            if dropped > 0 { body += "\n(+\(dropped) earlier lines dropped)" }
            // dead: the target refused an earlier delivery
            let dead = lock.withLock { () -> Bool in
                if error != nil { return true }
                delivered += 1
                return false
            }
            if dead { return }
            let text = header(ended: ended) + "\n" + body
            let target = AgentId(args.notify)
            kernel.spawnTask(agent: owner, verb: "monitor_notify") { [self] in
                let out = await kernel.send([
                    Item(target, .object(["type": .string("send"), "text": .string(text)]))
                ])
                let got = out.first?.result ?? .null
                guard case .string(let reason) = got["reason"], MONITOR_REFUSED.contains(reason)
                else { return }
                let why: String
                if case .string(let e) = got["error"] { why = e } else { why = reason }
                let first = lock.withLock { () -> Bool in
                    guard error == nil else { return false }
                    error = why
                    delivered = max(0, delivered - 1)
                    return true
                }
                if first { wake() }
            }
        }

        /// One row of `monitors`.
        var listing: JSON {
            lock.withLock {
                .object([
                    "monitor_id": .integer(id), "job_id": .integer(jobId.value),
                    "notify": .string(args.notify), "match": args.pattern.map(JSON.string) ?? .null,
                    "every": args.every, "tail": .integer(args.tail),
                    "label": args.label.map(JSON.string) ?? .null,
                    "delivered": .integer(delivered), "pending": .integer(pending),
                    "status": .string(error == nil ? "running" : "dead"),
                    "error": error.map(JSON.string) ?? .null,
                ])
            }
        }
    }

    /// The live monitors of ONE bundle instance (one kernel), in start order.
    final class MonitorTable: @unchecked Sendable {
        private let lock = NSLock()
        private var all: [Monitor] = []

        /// File `m` unless its job has already exited. Under the same lock `jobEnded`
        /// reads, so a job that ends right after the check still finds it: a job ends
        /// only after its process has (`alive` false from then on).
        func add(_ m: Monitor, ifAlive alive: () -> Bool) -> Bool {
            lock.withLock {
                guard alive() else { return false }
                all.append(m)
                return true
            }
        }

        func remove(_ m: Monitor) {
            lock.withLock { all.removeAll { $0 === m } }
        }

        private func of(_ owner: AgentId, _ jobId: TaskId) -> [Monitor] {
            lock.withLock { all.filter { $0.owner == owner && $0.jobId == jobId && !$0.isDead } }
        }

        func feedLine(_ owner: AgentId, _ jobId: TaskId, _ line: String) {
            for m in of(owner, jobId) { m.feedLine(line) }
        }

        func feedText(_ owner: AgentId, _ jobId: TaskId, _ text: String) {
            for m in of(owner, jobId) { m.feedText(text) }
        }

        func jobEnded(_ owner: AgentId, _ jobId: TaskId) {
            for m in of(owner, jobId) { m.jobEnded() }
        }

        func owned(_ owner: AgentId, _ id: Int64) -> Monitor? {
            lock.withLock { all.first { $0.owner == owner && $0.monitorId == id } }
        }

        /// `monitors` — this agent's monitors, a dead one with its `error`.
        func listing(_ owner: AgentId) -> JSON {
            let mine = lock.withLock { all.filter { $0.owner == owner && $0.monitorId != 0 } }
            return .object(["monitors": .array(mine.map(\.listing))])
        }
    }

    extension ExecBridgeBundle {
        /// `monitor` past its job lookup: the args, the fork, the filing — python
        /// `_monitor_verb`.
        func startMonitor(
            _ agentId: AgentId, jobId: TaskId, child: Child, payload: JSON, kernel: Kernel
        ) -> JSON {
            let args: MonitorArgs
            switch MonitorArgs.parse(payload, kernel: kernel) {
            case .failure(let r): return r.json
            case .success(let a): args = a
            }
            let m = Monitor(
                owner: agentId, jobId: jobId, args: args,
                caller: KernelTaskLocals.currentOrigin?.value,
                call: KernelTaskLocals.currentTask?.value)
            guard monitorTable.add(m, ifAlive: { child.isAlive }) else {
                return Refusal.bad("monitor", "job_id", "a RUNNING job — this one has exited")
            }
            let table = monitorTable
            let mid = kernel.spawnTask(agent: agentId, verb: "monitor") {
                await m.run(kernel: kernel, table: table)
            }
            m.arm(mid)
            return .object([
                "monitor_id": .integer(mid.value), "job_id": .integer(jobId.value),
                "notify": .string(args.notify), "match": args.pattern.map(JSON.string) ?? .null,
                "every": args.every, "tail": .integer(args.tail),
                "label": args.label.map(JSON.string) ?? .null,
            ])
        }

        /// `unmonitor {monitor_id}` — end one of this agent's monitors.
        func unmonitorReply(_ agentId: AgentId, _ payload: JSON, kernel: Kernel) async -> JSON {
            let raw = payload["monitor_id"]
            guard case .integer(let mid) = raw else {
                if raw.isNull { return Refusal.missing("unmonitor", "monitor_id") }
                return Refusal.bad("unmonitor", "monitor_id", "an int — the id `monitor` returned")
            }
            guard let m = monitorTable.owned(agentId, mid) else {
                return Refusal.make(
                    Refusal.notFound, "exec_bridge: no monitor \(mid)",
                    extra: [("monitor_id", raw)])
            }
            if m.isDead {  // its task is over, only the record is left
                monitorTable.remove(m)
                return .object(["unmonitored": .bool(true), "monitor_id": .integer(mid)])
            }
            _ = await kernel.cancelTask(TaskId(mid))
            return .object(["unmonitored": .bool(true), "monitor_id": .integer(mid)])
        }
    }

#endif  // os(macOS)
