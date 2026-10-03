// THE ROW EVENT — the ai_core verbs `watch` / `unwatch`.
//
// Port of python `ai_core/core.py` (canonical): `_watch`, `_unwatch`. The kernel
// holds no rows, so an agent announces its own rows to its watchers (`watch`,
// gated by the agent's own call gate). The event is a content-free digest, plus
// the content for a `body:true` watcher.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

extension AIBackend {
    /// args: body:bool? (default false), watcher:str? (operator only — an inbox id
    /// to deliver to). Announce every row this agent appends to YOUR inbox as
    /// {type:'row', source, row:{id, kind, role, bytes}} — content-free; with
    /// body:true the row's `content` too. Needs a registered inbox. Returns
    /// {watching:true, watcher, body}. Python `_watch`.
    func watchVerb(agent: Agent, payload: JSON, kernel: Kernel) -> JSON {
        let caller = KernelTaskLocals.currentOrigin?.value
        var watcher: String
        if let caller {
            let w = payload["watcher"]
            if w != .null && w != .string(caller) {
                return .object([
                    "error": .string("watch: an agent watches for ITSELF — `watcher` is the operator's"),
                    "reason": .string("unauthorized"),
                ])
            }
            watcher = caller
        } else {
            guard case .string(let w) = payload["watcher"], !w.isEmpty else {
                return .object([
                    "error": .string("watch: the operator must name a `watcher` inbox"),
                    "reason": .string("missing_arg"), "arg": .string("watcher"),
                ])
            }
            watcher = w
        }
        guard kernel.hasInbox(AgentId(watcher)) else {
            return .object([
                "error": .string(
                    "watch: \(pyRepr(watcher)) has no registered inbox, so nothing would read the row events"),
                "reason": .string("no_consumer"),
                "hint": .string(
                    "register the reader first — ctx.ensure_inbox(\(pyRepr(watcher))) — then watch"),
            ])
        }
        let body = pyTruthy(payload["body"])
        setWatcher(agent.id.value, kernel, watcher, body: body)
        return .object([
            "watching": .bool(true), "watcher": .string(watcher), "body": .bool(body),
        ])
    }

    /// args: watcher:str? (operator only). Stop announcing rows to you. Returns
    /// {watching:false}. Python `_unwatch`.
    func unwatchVerb(agent: Agent, payload: JSON, kernel: Kernel) -> JSON {
        let watcher: JSON =
            KernelTaskLocals.currentOrigin.map { JSON.string($0.value) } ?? payload["watcher"]
        if case .string(let w) = watcher { setWatcher(agent.id.value, kernel, w, body: nil) }
        return .object(["watching": .bool(false), "watcher": watcher])
    }
}
