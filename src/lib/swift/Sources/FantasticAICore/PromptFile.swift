// The system block from a FILE (owner, 2026-10-08): `system_prompt_file: {bridge,
// path}`. Python `ai_core/prompt_file.py`, text for text.
//
// The agent reads the file THROUGH that file_bridge, AS ITSELF — every read and
// watch is its own gated send, so its `can_call` names the bridge (`read_stream`,
// `watch_path`, `unwatch_path`, `watch`) and the bridge's `callable_by` names it. It
// holds the path under the bridge's disk watcher (`watch_path`) and watches the
// bridge through a private inbox: a `changed` event for the path marks the text
// STALE, and the next turn reads it again — no restart, and no read on a turn
// nothing changed. RAM only: armed on the first turn that needs it, re-armed when
// the record names another file, dropped when the field is cleared or the agent
// deleted. A file that cannot be read leaves the turn on the block below it, and the
// reason is the agent's `system_prompt_file_error` until a read succeeds.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public enum PromptFile {
    /// The refusal for a `system_prompt_file` the record cannot use.
    public static let REFUSAL =
        "system_prompt_file must be {bridge: <file_bridge id>, path: <path under its root>} — null clears it"

    final class Stale: @unchecked Sendable {
        private let lock = NSLock()
        private var value = true
        func set() { lock.withLock { value = true } }
        func take() -> Bool {
            lock.withLock {
                defer { value = false }
                return value
            }
        }
    }

    struct State {
        var file: (bridge: String, path: String)
        var text: String?
        var stale: Stale
        var error: String?
        var inbox: AgentId
        var task: TaskId?
    }

    nonisolated(unsafe) private static var states: [String: State] = [:]
    private static let lock = NSLock()

    private static func key(_ kernel: Kernel, _ id: AgentId) -> String {
        "\(kernel.serial)|\(id.value)"
    }

    private static func get(_ k: String) -> State? { lock.withLock { states[k] } }
    private static func put(_ k: String, _ s: State?) { lock.withLock { states[k] = s } }

    /// `(bridge, path)` the record names, or nil.
    public static func refOf(_ v: JSON?) -> (bridge: String, path: String)? {
        guard let m = v?.asObject, case .string(let b)? = m["bridge"], !pyStrip(b).isEmpty,
            case .string(let p)? = m["path"], !pyStrip(p).isEmpty
        else { return nil }
        return (b, p)
    }

    /// Why a written `system_prompt_file` is unusable, or nil (null clears it).
    public static func shapeError(_ v: JSON?) -> String? {
        guard let v, v != .null else { return nil }
        guard let m = v.asObject, m.count == 2, m["bridge"] != nil, m["path"] != nil,
            refOf(v) != nil
        else { return REFUSAL }
        return nil
    }

    /// Why the file is not this agent's block right now, or nil.
    public static func errorOf(_ kernel: Kernel, _ id: AgentId) -> String? {
        get(key(kernel, id))?.error
    }

    /// python `posixpath.normpath` for a path under a root.
    static func normpath(_ p: String) -> String {
        var out: [Substring] = []
        for seg in p.split(separator: "/", omittingEmptySubsequences: false) {
            switch seg {
            case "", ".": continue
            case "..":
                if let last = out.last, last != ".." { out.removeLast() } else { out.append(seg) }
            default: out.append(seg)
            }
        }
        return out.isEmpty ? "." : out.joined(separator: "/")
    }

    /// The file's text for this turn, or nil (no file named, or unreadable). Runs
    /// in the agent's own turn, so every send here is the agent's.
    static func text(_ agent: Agent, _ kernel: Kernel) async -> String? {
        let k = key(kernel, agent.id)
        let wanted = refOf(agent.metaValue(forKey: "system_prompt_file"))
        let held = get(k)
        guard let file = wanted else {
            if held != nil { await drop(kernel, agent.id) }
            return nil
        }
        if let h = held, h.file != file {
            await drop(kernel, agent.id)
            await arm(agent.id, kernel, file)
        } else if held == nil || held?.task == nil {
            // arming failed on an earlier turn — the same one path, again
            await arm(agent.id, kernel, file)
        }
        guard let st = get(k) else { return nil }
        if st.task != nil && st.stale.take() {
            let (txt, err) = await read(kernel, file.bridge, file.path)
            if var s = get(k) {
                s.text = txt
                s.error = err
                put(k, s)
            }
        }
        return get(k)?.text
    }

    private static func failed(_ out: Outcome) -> Bool {
        out.status != .completed || out.result.asObject == nil || pyTruthy(out.result["error"])
    }

    private static func why(_ out: Outcome) -> String {
        pyTruthy(out.result["error"]) ? pyStr(out.result["error"]) : out.status.rawValue
    }

    private static func arm(_ id: AgentId, _ kernel: Kernel, _ file: (bridge: String, path: String))
        async
    {
        let k = key(kernel, id)
        let inbox = AgentId("\(id.value)/prompt_file")
        let stale = Stale()
        put(k, State(file: file, text: nil, stale: stale, error: nil, inbox: inbox, task: nil))
        func fail(_ e: String) {
            if var s = get(k) {
                s.error = e
                put(k, s)
            }
        }
        let out = await kernel.send([
            Item(AgentId(file.bridge), .object(["type": "watch_path", "path": .string(file.path)]))
        ])[0]
        if failed(out) {
            fail("\(file.bridge) watch_path \(file.path): \(why(out))")
            return
        }
        let q = kernel.ensureInbox(inbox)
        if let denial = kernel.watch(src: AgentId(file.bridge), watcher: inbox) {
            kernel.removeInbox(inbox)
            fail("\(file.bridge) watch: \(pyStr(denial["error"]))")
            _ = await kernel.send([
                Item(
                    AgentId(file.bridge),
                    .object(["type": "unwatch_path", "path": .string(file.path)]))
            ])
            return
        }
        let norm = normpath(file.path)
        let tid = kernel.spawnTask(agent: id, verb: "prompt_file") {
            while let item = await q.next() {
                let p = item.payload
                if p["type"] == "changed", let path = p["path"].asString, normpath(path) == norm {
                    stale.set()
                }
            }
        }
        if var s = get(k) {
            s.task = tid
            put(k, s)
        }
    }

    /// The whole file through `read_stream`, as utf-8 → (text, nil) or (nil, why).
    private static func read(_ kernel: Kernel, _ bridge: String, _ path: String) async -> (
        String?, String?
    ) {
        var bytes = Data()
        var offset: Int64 = 0
        while true {
            let out = await kernel.send([
                Item(
                    AgentId(bridge),
                    .object([
                        "type": "read_stream", "path": .string(path), "offset": .integer(offset),
                    ]),
                    rawReply: true)
            ])[0]
            if failed(out) { return (nil, "\(bridge) read_stream \(path): \(why(out))") }
            if let b = out.body?.at("bytes") { bytes.append(b) }
            if out.result["eof"] == .bool(true) { break }
            offset = out.result["next_offset"].asInt ?? offset
        }
        guard let t = String(data: bytes, encoding: .utf8) else {
            return (nil, "\(bridge) \(path): not utf-8")
        }
        return (t, nil)
    }

    /// Stop watching: the drain task, the watch, the inbox, the bridge's path hold.
    public static func drop(_ kernel: Kernel, _ id: AgentId) async {
        let k = key(kernel, id)
        guard let st = get(k) else { return }
        put(k, nil)
        guard let tid = st.task else { return }
        _ = await kernel.cancelTask(tid)
        kernel.unwatch(src: AgentId(st.file.bridge), watcher: st.inbox)
        kernel.removeInbox(st.inbox)
        _ = await kernel.send([
            Item(
                AgentId(st.file.bridge),
                .object(["type": "unwatch_path", "path": .string(st.file.path)]))
        ])
    }
}
