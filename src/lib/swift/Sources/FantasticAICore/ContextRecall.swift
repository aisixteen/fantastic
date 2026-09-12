// `context_recall` — rebuild this agent's VIEW from its store: fit the head to
// the budget at digest resolution, then ask the model which rows it wants in
// full, persist the id list, log the decision as a row, and drop RAM scratch.
// Runs on boot, after a fold batch, or on request.
//
// Plus the two pieces of RAM that gate it: the fold-count debounce
// (`RECALL_EVERY`) and the per-agent INCARNATION a late reply is checked
// against. Python `ai_core/core.py` (`_context_recall`, `_RECALL_PROMPT`,
// `_first_json`, `_recall_due`, `_folds_since`, `_incarnation`, `_make_boot`);
// rust `retrieval.rs`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Fold rows between automatic recalls. Python `RECALL_EVERY`.
let RECALL_EVERY = 3

/// Python `_RECALL_PROMPT`, verbatim; `{budget}`, `{used}`, `{done}` and
/// `{lines}` are filled in by `contextRecall`.
let RECALL_PROMPT = """
    You are rebuilding your own working context. Below is the HEAD of your
    conversation as one line per row: id · kind · title · size · ↑parent. Fold rows stand for
    finished stretches (their members are one descent away). Everything is currently at
    DIGEST resolution; your budget for the body is {budget} tokens and the digests use
    {used}. Replies ALREADY IN THIS HISTORY (their calls need not be repeated; the task itself may be unfinished):
    {done}
    Choose which rows you want IN FULL next turn (expansions cost their size) and say in one
    sentence why. Reply JSON only:
    {"expand": [<id>, ...], "why": "<one sentence>"}

    {lines}
    """

/// INCARNATION per agent id, bumped on every boot. A reply dispatched under one
/// incarnation must not become a live turn of the next: after a reboot the
/// rebuilt agent would be woken by answers to calls it no longer remembers
/// making. Measured 2026-09-06 (python, reboot_continuity): the rebooted agent
/// received phase-one read replies, went back to "verifying" work already
/// folded, and never did the new instruction. Python `_incarnation` — one table
/// for the whole process, keyed by agent id, because delivery is a free
/// function that knows only the target's id.
enum AIIncarnation {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var table: [String: Int] = [:]

    static func of(_ id: String) -> Int {
        lock.lock()
        defer { lock.unlock() }
        return table[id] ?? 0
    }

    @discardableResult
    static func bump(_ id: String) -> Int {
        lock.lock()
        defer { lock.unlock() }
        let n = (table[id] ?? 0) + 1
        table[id] = n
        return n
    }

    /// TEST SEAM — python's `_incarnation["ai"] = 1`.
    static func set(_ id: String, _ n: Int) {
        lock.lock()
        table[id] = n
        lock.unlock()
    }
}

/// The first balanced `{…}` object in `text`, parsed. Python `_first_json`.
func firstJson(_ text: String) -> OrderedDictionary<String, JSON>? {
    let chars = Array(text)
    guard let i = chars.firstIndex(of: "{") else { return nil }
    var depth = 0
    for j in i..<chars.count {
        depth += (chars[j] == "{" ? 1 : 0) - (chars[j] == "}" ? 1 : 0)
        if depth == 0 {
            guard let out = try? JSON.parse(String(chars[i...j])) else { return nil }
            return out.asObject
        }
    }
    return nil
}

/// Python's `repr()` of a str — single quotes unless the text holds a single
/// quote and no double quote; backslashes and the quote escaped. Enough for
/// the one place a python error message interpolates `{name!r}`.
func pyRepr(_ s: String) -> String {
    let q: Character = (s.contains("'") && !s.contains("\"")) ? "\"" : "'"
    var out = String(q)
    for c in s {
        if c == "\\" { out += "\\\\" } else if c == q { out += "\\" + String(q) } else if c == "\n" {
            out += "\\n"
        } else if c == "\r" { out += "\\r" } else if c == "\t" { out += "\\t" } else { out.append(c) }
    }
    out.append(q)
    return out
}

/// Python's `str(x)` for a JSON value: a string as itself, `None` for null,
/// otherwise the serialized form. (`True`/`False` for bools, as python spells them.)
func pyStr(_ v: JSON) -> String {
    switch v {
    case .string(let s): return s
    case .null: return "None"
    case .bool(let b): return b ? "True" : "False"
    default: return v.serialize()
    }
}

extension AIBackend {
    // MARK: - RAM the recall gates

    /// `_recall_due.discard(id)` — true iff a recall was owed.
    func takeRecallDue(_ key: String) -> Bool {
        projectionLock.lock()
        defer { projectionLock.unlock() }
        return recallDue.remove(key) != nil
    }

    /// TEST SEAM: is a recall owed at the start of the next turn?
    func isRecallDue(_ key: String) -> Bool {
        projectionLock.lock()
        defer { projectionLock.unlock() }
        return recallDue.contains(key)
    }

    /// `_folds_since[id] += n`; every `RECALL_EVERY`-th batch marks a recall due.
    func countFolds(_ key: String, _ n: Int) {
        projectionLock.lock()
        defer { projectionLock.unlock() }
        let since = (foldsSince[key] ?? 0) + n
        if since >= RECALL_EVERY {
            foldsSince[key] = 0
            recallDue.insert(key)
        } else {
            foldsSince[key] = since
        }
    }

    func setViewSource(_ key: String, _ source: String) {
        projectionLock.lock()
        viewSource[key] = source
        projectionLock.unlock()
    }

    func getViewSource(_ key: String) -> String? {
        projectionLock.lock()
        defer { projectionLock.unlock() }
        return viewSource[key]
    }

    /// RAM is scratch, reconstructible by definition; the store is never cleared.
    private func dropScratch(_ key: String) {
        projectionLock.lock()
        projectionCache[key] = nil
        compactionMark[key] = nil
        projectionLock.unlock()
        AIStripStrategy.forget(key)
    }

    // MARK: - the model call

    /// One plain completion, reasoning stripped (see `makeSummarizer`). Python
    /// `_compose`.
    func compose(_ provider: any AIProvider, _ prompt: String) async throws -> String {
        var parts = ""
        for try await chunk in provider.chat(messages: [
            .object(["role": .string("user"), "content": .string(prompt)])
        ]) {
            if case .token(let t) = chunk { parts += t }
        }
        return withoutThinking(parts)
    }

    /// The provider for a side call (boot's recall, the `context_recall` verb) —
    /// a refusal (no api_key, model unavailable) is `nil`: the recall then stays
    /// at the mechanical fit, exactly as python's `_get_provider` failure does.
    func sideProvider(agent: Agent, kernel: Kernel) async -> (any AIProvider)? {
        switch await config.makeProvider(agent, "cli", kernel) {
        case .provider(let p): return p
        case .refused: return nil
        }
    }

    // MARK: - the verb

    /// args: dry:bool? (mechanical fit only — no model call). Rebuild this
    /// agent's VIEW from its store: fit the head to the budget at digest
    /// resolution, then ask the model which rows it wants in full, persist the
    /// id list, log the decision as a row, and drop RAM scratch. Runs on boot,
    /// after a fold batch, or on request. Returns {kept, expanded, dropped,
    /// budget, why, degraded}. Python `_context_recall`.
    func contextRecall(
        agent: Agent, payload: JSON, kernel: Kernel, provider: (any AIProvider)?
    ) async -> JSON {
        let key = agent.id.value
        let rows = loadHistory(agent: agent, kernel: kernel)
        if rows.isEmpty {
            return .object([
                "kept": .integer(0), "expanded": .integer(0), "dropped": .integer(0),
                "why": .string("empty store"), "degraded": .bool(false),
            ])
        }
        let sysTokens = AIContext.estimateOne(
            .object([
                "role": .string("system"),
                "content": .string(
                    await assembleSystemPrompt(agent: agent, systemPrompt: nil, kernel: kernel)),
            ]))
        let bodyBudget = max(
            AIContext.budgetFloor, AIContext.budget(agent) - sysTokens - AIContext.noticeEnvelopeReserve)
        let target = Int64(Double(bodyBudget) * AIStripStrategy.LOW)
        let view = AIFolds.render(rows).filter { rowId($0).map { $0 != 0 } ?? false }
        if view.isEmpty {
            return .object([
                "kept": .integer(0), "expanded": .integer(0), "dropped": .integer(0),
                "why": .string("no addressable rows"), "degraded": .bool(false),
            ])
        }
        func digest(_ r: JSON) -> JSON {
            resOf(r) == "full" && rowContent(r).hasPrefix("[reply to ") ? applyRes(r, "stub") : r
        }
        func cost(_ r: JSON) -> Int64 {
            AIContext.estimateOne(
                .object([
                    "role": r["role"].asString.map(JSON.string) ?? .string("user"),
                    "content": .string(rowContent(r)),
                ]))
        }
        // 1. MECHANICAL FIT — never fails. The goal turn always in; then newest first.
        var picked: Set<Int> = [0]
        var used = cost(digest(view[0]))
        for i in stride(from: view.count - 1, to: 0, by: -1) {
            let c = cost(digest(view[i]))
            if used + c > target { break }
            picked.insert(i)
            used += c
        }
        let head = picked.sorted()
        // 2. ONE MODEL CALL — the model chooses expansions; a bad answer changes nothing.
        var why = "mechanical fit"
        var degraded = true
        var expand: Set<Int64> = []
        let dry = payload["dry"].asBool ?? false
        if let provider, !dry {
            var lines: [String] = []
            for i in head {
                let r = view[i]
                let content = rowContent(r)
                let title =
                    r["title"].asString.flatMap { $0.isEmpty ? nil : $0 }
                    ?? AIFolds.quotedTitle(content)
                    ?? String(content.unicodeScalars.prefix(80)).replacingOccurrences(of: "\n", with: " ")
                let kind = r["kind"].asString.flatMap { $0.isEmpty ? nil : $0 } ?? rowRole(r)
                let parent = rowParent(r).flatMap { $0 == 0 ? nil : String($0) } ?? "-"
                lines.append(
                    "\(rowId(r) ?? 0) · \(kind) · \"\(title)\" · \(pyLen(content))B · ↑\(parent)")
            }
            // what the store already answers: every reply row names the call it
            // closed, so a rebooted model does not re-run reads whose results it is
            // about to see
            var done: [String] = []
            for r in view {
                let content = rowContent(r)
                guard let tag = parseReplyTag(content) else { continue }
                var line = "\(tag.agent) \(tag.verb)"
                if let t = AIFolds.quotedTitle(content) { line += " · \"\(t)\"" }
                if !done.contains(line) { done.append(line) }
            }
            let doneText = done.isEmpty ? "(nothing yet)" : done.suffix(40).joined(separator: "\n")
            let prompt = RECALL_PROMPT
                .replacingOccurrences(of: "{budget}", with: String(bodyBudget))
                .replacingOccurrences(of: "{used}", with: String(used))
                .replacingOccurrences(of: "{done}", with: doneText)
                .replacingOccurrences(of: "{lines}", with: lines.joined(separator: "\n"))
            let ans: OrderedDictionary<String, JSON>?
            do { ans = firstJson(try await compose(provider, prompt)) } catch { ans = nil }
            if let ans, !ans.isEmpty {
                let headIds = Set(head.compactMap { rowId(view[$0]) })
                let want: [Int64] = (ans["expand"]?.asArray ?? []).compactMap { asMessageId($0) }
                    .filter { headIds.contains($0) }
                let whyText: String = {
                    guard let w = ans["why"], w != .null, w != .bool(false) else { return "" }
                    return pyStr(w)
                }()
                let cutWhy = String(whyText.unicodeScalars.prefix(300))
                why = cutWhy.isEmpty ? "chosen" : cutWhy
                degraded = false
                // charge each expansion at full size, newest first; drop what does not fit
                var total = used
                for x in want.reversed() {
                    guard let r = view.first(where: { rowId($0) == x }) else { continue }
                    let extra = cost(r) - cost(digest(r))
                    if total + extra <= bodyBudget {
                        expand.insert(x)
                        total += extra
                    }
                }
            }
        }
        var ids: [JSON] = []
        for i in head {
            let r = view[i]
            let id = rowId(r) ?? 0
            let res: String
            if r["kind"].asString == "fold" {
                res = "fold"
            } else if expand.contains(id) || !rowContent(r).hasPrefix("[reply to ") {
                res = "full"
            } else {
                res = "stub"
            }
            ids.append(.object(["id": .integer(id), "res": .string(res)]))
        }
        saveView(agent.id, kernel: kernel, ids: ids, budget: bodyBudget, why: why)
        // the decision is a ROW — the next reload reads why the last recall chose as it did
        let rid = kernel.rows.mintId()
        // THE LIVE TASK IS NAMED. The view keeps the first root (the goal) at the
        // top, and after a recall a model with several roots re-answered the FIRST
        // one and called done (measured 2026-09-06, python fold_paging: TASK 0
        // re-answered at turn 88 with TASK 15 and a FINAL pending). The newest root
        // is what is live; say so. Over the STORE, not the folded render: a root
        // that a fold collapsed is still the live one if nothing newer came
        // (measured 2026-09-06, folds_x_chains: TASK C folded, the row named TASK A).
        let live = rows.last { r in
            rowRole(r) == "user"
                && (rowParent(r).map { $0 == 0 } ?? true)
                && (r["kind"].asString.map { $0.isEmpty || $0 == "user" } ?? true)
                && parseReplyTag(rowContent(r)) == nil
        }
        let liveTxt: String = live.map { l in
            let text = String(
                rowContent(l).split(whereSeparator: { $0.isWhitespace }).joined(separator: " ")
                    .unicodeScalars.prefix(100))
            return " · live task: \(rowId(l) ?? 0) \"\(text)\""
        } ?? ""
        appendRows(
            agent.id, kernel: kernel,
            [
                .object([
                    "role": .string("user"),
                    "id": .integer(rid),
                    "parent": .null,
                    "kind": .string("recall"),
                    "title": .string(why),
                    "content": .string(
                        "[recall \(rid) · kept \(ids.count) rows, \(expand.count) in full] \(why)\(liveTxt)"),
                ])
            ])
        dropScratch(key)
        return .object([
            "kept": .integer(Int64(ids.count)),
            "expanded": .integer(Int64(expand.count)),
            "dropped": .integer(Int64(view.count - ids.count)),
            "budget": .integer(bodyBudget),
            "why": .string(why),
            "degraded": .bool(degraded),
        ])
    }
}
