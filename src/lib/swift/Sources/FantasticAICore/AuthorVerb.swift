// author — the AI agent's SELF-REFLECT / SELF-CORRECT loop, as a verb.
//
// Merged INTO ai-core (2026-08-07, user decision): the verify-and-repair driver
// is the AI agent driving ITSELF. Python `ai_core/author.py` is canonical; on
// disagreement this file is wrong. `{type:'author', goal}` makes the agent
// author a durable tool on exec_bridge, OBSERVE it answering a probe the model
// never sees, and refuse its own `final` until reality agrees.

import FantasticJSON
import FantasticKernel
import Foundation

let AUTHOR_LEAN_SYSTEM = """
    You are an agent inside a LIVE agent kernel. Everything here is an agent addressed by a string id. There is ONE primitive: send([Item(target_id, payload)])[0].result — a JSON payload whose "type" field names the verb; the target replies with JSON.

    You act by replying with EXACTLY ONE JSON object and NOTHING else, in one of these two forms:
      {"act": {"target": "<id>", "payload": { ...a verb payload... }}}
      {"final": "<short message>"}
    After an {"act"} I send it for you and reply with the result as your next message.

    YOUR REPLY MUST BE ONE COMPLETE, WELL-FORMED JSON OBJECT. It is parsed by a machine, not read by a human: nothing is repaired for you. Every "{" you open you MUST close with "}", and every "[" with "]" — an object that ends one brace short is DISCARDED and your act never happens. No prose, no markdown, no code fences, no trailing commas. When the payload carries source code, the code is ONE JSON string: escape every " as \\" and every newline as \\n, and close the string before you close the enclosing objects. Before you finish, re-read your reply from "{" to the final "}" and confirm it is balanced.

    To LEARN anything, reflect it: {"act":{"target":"<id>","payload":{"type":"reflect"}}}. Reflecting "kernel" returns the system overview, your OWN capabilities, the agent tree, and a MAP of keyed docs (`readmes`) you can fetch one at a time. DISCOVER what you need by reflecting BEFORE you act — do not assume verb or API shapes.

    You are ALONE and AUTONOMOUS — no human will reply between your turns. Never stop to ask, and never announce that you are "ready to proceed" or "understand now": that is NOT done. Reflecting and planning are not completing. Every reply must be a concrete {"act"} that moves the task forward. Only reply {"final":...} once the thing you were asked to build actually EXISTS and you have exercised it and seen it work. If you catch yourself about to declare readiness, instead emit the next {"act"} (a create_agent, or a send). Do NOT reflect the same target or doc twice — reading is not progress. The MOMENT a doc shows you how to build the tool (e.g. the self-extension doc's copy-pasteable example), your NEXT act MUST be a create_agent, not another reflect.
    """

/// The FIRST complete JSON object in `s`, or nil. NO repair — a driver that
/// patches the envelope is measuring its own patch.
func authorFirstJSON(_ s: String) -> JSON? {
    guard let start = s.firstIndex(of: "{") else { return nil }
    let tail = String(s[start...])
    // Walk balanced braces respecting strings, then parse the prefix.
    var depth = 0
    var inString = false
    var escaped = false
    for (i, ch) in tail.enumerated() {
        if escaped { escaped = false; continue }
        switch ch {
        case "\\" where inString: escaped = true
        case "\"": inString.toggle()
        case "{" where !inString: depth += 1
        case "}" where !inString:
            depth -= 1
            if depth == 0 {
                let idx = tail.index(tail.startIndex, offsetBy: i + 1)
                let candidate = String(tail[..<idx])
                if let obj = try? JSON.parse(candidate), obj.asObject != nil {
                    return obj
                }
                return try? JSON.parse(candidate.replacingOccurrences(of: "\\'", with: "'"))
            }
        default: break
        }
    }
    return nil
}

extension AIBackend {
    /// `{type:'author', goal, steps?, id?, probe?, must_call?, expect?}` — the
    /// agent's SELF-CORRECT loop (py-canonical `ai_core/author.py`). Returns
    /// `{id, code, message}` once the tool is OBSERVED answering, else `{error}`.
    func authorVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        guard let goal = payload["goal"].asString, !goal.trimmingCharacters(in: .whitespaces).isEmpty
        else { return .object(["error": .string("author: goal (str) required")]) }
        let steps = min(Int(payload["steps"].asInt ?? 26), 64)
        let probe = payload["probe"].asString ?? "hello world"
        let expect = payload["expect"].asString ?? ""
        let wantId = payload["id"].asString
        let probeWait = Double(
            ProcessInfo.processInfo.environment["FANTASTIC_AUTHOR_PROBE_WAIT"] ?? "1.5") ?? 1.5
        let bootBeat = Double(
            ProcessInfo.processInfo.environment["FANTASTIC_AUTHOR_BOOT_BEAT"] ?? "0.4") ?? 0.4

        let earId = AgentId("_author_ear_\(agent.id.value)")
        let earStream = kernel.ensureInbox(earId)
        final class Ear: @unchecked Sendable {
            let lock = NSLock()
            var says: [String] = []
            var sawDone = false
            var crash: String?
            func add(_ ev: JSON) {
                lock.withLock {
                    switch ev["type"].asString {
                    case "say": says.append(ev["text"].asString ?? "")
                    case "done": sawDone = true
                    case "job_done" where ev["ok"].asBool == false:
                        crash = ev["error"].asString ?? ev["stderr"].asString
                        sawDone = true
                    default: break
                    }
                }
            }
            func reset() { lock.withLock { says = []; sawDone = false } }
            func said() -> String { lock.withLock { says.last ?? "" } }
            func answered() -> Bool { lock.withLock { !says.isEmpty } }
            func done() -> Bool { lock.withLock { sawDone } }
            func takeCrash() -> String? {
                lock.withLock {
                    let c = crash
                    crash = nil
                    return c
                }
            }
        }
        let ear = Ear()
        let earTask = Task { for await ev in earStream { ear.add(ev) } }
        defer { earTask.cancel() }

        func scrub(_ t: String) -> String {
            var out = t.replacingOccurrences(
                of: probe, with: "<a probe turn you never see>", options: .caseInsensitive)
            if !expect.isEmpty {
                out = out.replacingOccurrences(
                    of: expect, with: "<redacted>", options: .caseInsensitive)
            }
            return out
        }

        func probeTool(_ tool: String) async -> Bool {
            ear.reset()
            let refl = await kernel.send([Item(AgentId(tool), .object(["type": .string("reflect")]))])[0].result
            if refl["running"].asInt == 0,
                kernel.agent(AgentId(tool))?.metaValue(forKey: "code")?.asString?.isEmpty == false
            {
                _ = await kernel.send([Item(AgentId(tool), .object(["type": .string("start")]))])[0].result
                try? await Task.sleep(nanoseconds: UInt64(bootBeat * 1e9))
            }
            _ = await kernel.send([Item(AgentId(tool), .object([
                    "type": .string("send"), "text": .string(probe),
                    "client_id": .string("author"),
                ]))])[0].result
            let deadline = Date().addingTimeInterval(probeWait)
            while Date() < deadline {
                if ear.answered() || ear.done() { break }
                try? await Task.sleep(nanoseconds: 50_000_000)
            }
            if !ear.answered() && ear.takeCrash() == nil {
                // The ear can miss a create-time crash — the job table cannot.
                let st = await kernel.send([Item(AgentId(tool), .object(["type": .string("status")]))])[0].result
                if let jobs = st["jobs"].asArray,
                    let failed = jobs.last(where: { $0["status"].asString == "failed" })
                {
                    let jid = failed["job_id"].asString ?? ""
                    let full = await kernel.send([Item(AgentId(tool), .object(["type": .string("status"), "job_id": .string(jid)]))])[0].result
                    let text =
                        full["jobs"].asArray?.first?["stderr"].asString
                        ?? failed["last_line"].asString
                    if let t = text, !t.isEmpty {
                        ear.lock.withLock { ear.crash = t }
                    }
                }
            }
            return ear.answered()
        }

        func canaryOK() -> Bool {
            if expect.isEmpty { return true }
            let cleaned = ear.said().replacingOccurrences(
                of: probe, with: "", options: .caseInsensitive)
            return cleaned.range(of: expect, options: .caseInsensitive) != nil
        }

        var history: [JSON] = []
        var turn = authorTaskPrompt(goal: goal, wantId: wantId)
        var tool: String?
        var verified = false
        var finalMsg: String?
        var lastState = "no tool was ever created"

        for _ in 0..<steps {
            let scrubbed = scrub(turn)
            var msgs = history
            msgs.append(.object(["role": .string("user"), "content": .string(scrubbed)]))
            let reply = await kernel.send([Item(agent.id, .object([
                    "type": .string("send"), "text": .string(scrubbed),
                    "system_prompt": .string(AUTHOR_LEAN_SYSTEM),
                    "messages": .array(msgs),
                    "client_id": .string("author"),
                ]))])[0].result
            let response = reply["response"].asString ?? ""
            history.append(.object(["role": .string("user"), "content": .string(scrubbed)]))
            history.append(
                .object(["role": .string("assistant"), "content": .string(response)]))

            guard let obj = authorFirstJSON(response) else {
                turn =
                    "Your reply was not ONE complete JSON object, so NOTHING happened. "
                    + "Reply with exactly {\"act\":{...}} or {\"final\":\"...\"} and "
                    + "nothing else. Emit your next act now."
                continue
            }

            if obj["final"].asString != nil {
                if let t = tool, verified {
                    finalMsg = obj["final"].asString
                    _ = t
                    break
                }
                if let t = tool {
                    let ok = await probeTool(t)
                    if ok && canaryOK() {
                        verified = true
                        finalMsg = obj["final"].asString
                        break
                    }
                    if let crash = ear.takeCrash() {
                        lastState = "it CRASHED: \(crash.split(separator: "\n").last ?? "")"
                        turn =
                            "AUTONOMOUS VERIFY: your agent '\(t)' CRASHED handling my probe "
                            + "turn. The interpreter's own words:\n\(crash)\nFix exactly that "
                            + "(update_agent its `code` then stop+boot, or delete_agent then "
                            + "create_agent with corrected code). Emit your next act now."
                        continue
                    }
                    let refl = await kernel.send([Item(AgentId(t), .object(["type": .string("reflect")]))])[0].result
                    lastState = "it started but emitted no say/done (deaf)"
                    turn =
                        "You replied final, but I have NOT observed your tool answer the "
                        + "probe turn I sent it — you have NOT succeeded. Do not give up. "
                        + "AUTONOMOUS VERIFY: I sent your agent '\(t)' a probe turn of my own "
                        + "choosing and it started but emitted NO say/done — it is deaf, not "
                        + "broken. Its state: \(String(refl.serialize().prefix(1200))). Fix "
                        + "it: update_agent its `code` then stop+boot, or delete_agent then "
                        + "create_agent again. Emit your next act now."
                    continue
                }
                lastState = "no tool was ever created"
                turn =
                    "You replied final, but you have not created any tool — create_agent "
                    + "one NOW, on the exec_bridge executor, whose `code` satisfies the "
                    + "goal. Emit your next act now."
                continue
            }

            guard let act = obj["act"].asObject,
                let target = act["target"]?.asString,
                let actPayload = act["payload"], actPayload.asObject != nil
            else {
                turn =
                    "An {\"act\"} needs {\"target\":\"<id>\",\"payload\":{...}}. Yours was "
                    + "missing one of them. Emit your next act now."
                continue
            }
            // The tools an author builds are ITS OWN subtree. Route every
            // structural mutation through this agent's own id, so create_agent
            // lands the tool as a child of the author and later delete/update edit
            // it — all baseline authority (fork + $children), zero grants, and the
            // tools are owned + cleaned up with the author.
            let actVerb = actPayload["type"].asString ?? ""
            let dispatchTarget = privilegedVerbs.contains(actVerb) ? agent.id : AgentId(target)
            let result = await kernel.send([Item(dispatchTarget, actPayload)])[0].result
            if actPayload["type"].asString == "create_agent",
                actPayload["handler_module"].asString == "exec_bridge.tools",
                result["error"].asString == nil,
                let newId = result["id"].asString
            {
                tool = newId
                kernel.watch(src: AgentId(newId), watcher: earId)
                try? await Task.sleep(nanoseconds: UInt64(bootBeat * 1e9))
                verified = false
            }
            turn = "Result of your send to '\(target)':\n\(result.serialize())"

            if let t = tool {
                let ok = await probeTool(t)
                if ok && canaryOK() {
                    verified = true
                    turn +=
                        "\n\nAUTONOMOUS VERIFY: I probed your agent with a turn of my own "
                        + "and it ANSWERED. If you are done, reply "
                        + "{\"final\":\"<what you built>\"}."
                    continue
                }
                if let crash = ear.takeCrash() {
                    lastState = "it CRASHED: \(crash.split(separator: "\n").last ?? "")"
                    turn +=
                        "\n\nAUTONOMOUS VERIFY: your agent '\(t)' CRASHED handling my probe "
                        + "turn. The interpreter's own words:\n\(crash)\nFix exactly that. "
                        + "Emit your next act now."
                    continue
                }
                lastState = "it started but emitted no say/done (deaf)"
                turn +=
                    "\n\nAUTONOMOUS VERIFY: I probed your agent '\(t)' and it emitted NO "
                    + "say/done — it is deaf. Fix its code (update_agent then stop+boot, or "
                    + "delete_agent + create_agent). Emit your next act now."
            }
        }

        if let msg = finalMsg, let t = tool {
            let code = kernel.agent(AgentId(t))?.metaValue(forKey: "code")?.asString ?? ""
            return .object([
                "id": .string(t), "code": .string(code), "message": .string(msg),
            ])
        }
        return .object([
            "error": .string(
                "author: gave up after \(steps) steps — tool: \(tool ?? "none"); "
                    + "last state: \(lastState)")
        ])
    }
}

private func authorTaskPrompt(goal: String, wantId: String?) -> String {
    let nameClause = wantId.map { " Name it '\($0)'." } ?? ""
    return
        "TASK: Author a DURABLE tool — a new agent whose behaviour you write — that "
        + "satisfies this goal: \(goal).\(nameClause) It must ANSWER a turn: when it is "
        + "sent {\"type\":\"send\",\"text\":<text>,\"client_id\":<id>} it emits a 'say' "
        + "(tagged with that client_id) and then a 'done'. Discover HOW to author it "
        + "(which executor, its script API, its receive/emit pattern) by REFLECTING "
        + "first. Create it, then TEST it yourself by sending it "
        + "{\"type\":\"send\",\"text\":<any turn you like>,\"client_id\":\"me\"}. I will "
        + "ALSO probe it myself, every step, with a turn of MY choosing that you will "
        + "never see, and I will tell you what it did — so it must work for ANY turn, "
        + "not for one you prepared for. Only reply final once it really answers."
}
