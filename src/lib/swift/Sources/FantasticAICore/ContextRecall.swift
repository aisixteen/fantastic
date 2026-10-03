// `context_recall` — rebuild this agent's VIEW from its store: fit the head to
// the budget at digest resolution, then ask the model which rows it wants in
// full, persist the id list, log the decision as a row, and drop RAM scratch.
// Runs on boot, after a fold batch, or on request.
//
// Plus the pieces of RAM that gate it: the fold-count debounce
// (`AIConstants.RECALL_EVERY`) and the per-agent INCARNATION a late reply is checked
// against. Python `ai_core/core.py` (`_context_recall`, `_RECALL_PROMPT`,
// `_first_json`, `_recall_due`, `_folds_since`, `_incarnation`, `_make_boot`);
// rust `retrieval.rs`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

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
/// KEYED BY KERNEL, like `AIRunStates`: python's `_incarnation` is a module dict
/// in a one-kernel-per-process runtime, while swift hosts many kernels at once.
/// A flat id→int map let one kernel's `boot` bump another kernel's agent, and a
/// reply already in flight there came back as a LATE reply — recorded, never
/// acted on. The chain simply stopped, with nothing anywhere saying why.
enum AIIncarnation {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var table: [UInt64: [String: Int]] = [:]

    static func of(_ kernel: Kernel, _ id: String) -> Int {
        lock.lock()
        defer { lock.unlock() }
        return table[kernel.serial]?[id] ?? 0
    }

    @discardableResult
    static func bump(_ kernel: Kernel, _ id: String) -> Int {
        lock.lock()
        defer { lock.unlock() }
        let key = kernel.serial
        let n = (table[key]?[id] ?? 0) + 1
        table[key, default: [:]][id] = n
        return n
    }

    /// TEST SEAM — python's `_incarnation["ai"] = 1`.
    static func set(_ kernel: Kernel, _ id: String, _ n: Int) {
        lock.lock()
        table[kernel.serial, default: [:]][id] = n
        lock.unlock()
    }

    /// Drop a dead kernel's whole sub-table. Without it the map grew one entry
    /// per kernel the process ever built and nothing ever removed them.
    static func forget(_ kernel: Kernel) {
        lock.lock()
        table.removeValue(forKey: kernel.serial)
        lock.unlock()
    }
}

/// The process-wide AI side tables are keyed by `Kernel.serial`, so a kernel
/// that shuts down must take its rows with it. Registered ONCE, the first time
/// any AI backend is built — the kernel cannot reach into this module itself.
enum AISideTables {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var installed = false

    static func installTeardownHook() {
        lock.lock()
        defer { lock.unlock() }
        if installed { return }
        installed = true
        Kernel.onTeardown { kernel in
            AIIncarnation.forget(kernel)
            AIRunStates.forget(kernel)
            AIMemory.forget(kernel)
            AICalibration.forget(kernel)
            AISilentStreaks.forget(kernel)
            AIStripStrategy.forget(kernel)
        }
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

extension AIBackend {
    // MARK: - RAM the recall gates

    /// `_recall_due.discard(id)` — true iff a recall was owed.
    func takeRecallDue(_ key: String) -> Bool {
        projectionLock.lock()
        defer { projectionLock.unlock() }
        return recallDue.remove(key) != nil
    }

    /// `_recall_due.add(id)` — the next turn opens with `context_recall` (a boot,
    /// or enough folds).
    func markRecallDue(_ key: String) {
        projectionLock.lock()
        recallDue.insert(key)
        projectionLock.unlock()
    }

    /// A clear drops the fold counter, the owed recall and the view source.
    private func forgetClearState(_ key: String) {
        projectionLock.lock()
        foldsSince[key] = nil
        recallDue.remove(key)
        viewSource[key] = nil
        projectionLock.unlock()
    }

    /// TEST SEAM: is a recall owed at the start of the next turn?
    func isRecallDue(_ key: String) -> Bool {
        projectionLock.lock()
        defer { projectionLock.unlock() }
        return recallDue.contains(key)
    }

    /// `_folds_since[id] += n`; every `AIConstants.RECALL_EVERY`-th batch marks a recall due.
    func countFolds(_ key: String, _ n: Int) {
        projectionLock.lock()
        defer { projectionLock.unlock() }
        let since = (foldsSince[key] ?? 0) + n
        if since >= AIConstants.RECALL_EVERY {
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
    private func dropScratch(_ key: String, _ kernel: Kernel) {
        projectionLock.lock()
        projectionCache[key] = nil
        compactionMark[key] = nil
        projectionLock.unlock()
        AIStripStrategy.forget(kernel, key)
    }

    // MARK: - the clear

    /// args: why:str? — start a NEW THREAD on the same store: a CLEAR-FOLD — one
    /// row whose members are every row since the previous clear (no summary) — and
    /// the model's view begins at its `[cleared <id> · N rows]` line. Nothing is
    /// deleted or forgotten: grep, recall and `history {fold: <id>}` reach every
    /// member. Empties the persisted view and drops RAM scratch (projection, fold
    /// counters, pending recall). Returns {cleared:id, rows:N}. Python
    /// `_context_clear`.
    func contextClear(agent: Agent, payload: JSON, kernel: Kernel) async throws -> JSON {
        let key = agent.id.value
        let head = try await loadHead(key, kernel).filter {
            !BOOKKEEPING.contains($0["kind"].asString ?? "")
        }
        let members: [JSON] = head.compactMap { r in
            let id = r["id"]
            return id == .null ? nil : id
        }
        // `" ".join(str(payload.get("why") or "").split())[:200]` — python
        // TRUTHINESS, whitespace collapsed, cut on CODE POINTS.
        let raw = payload["why"]
        let collapsed = pySplitWhitespace(pyTruthy(raw) ? pyStr(raw) : "").joined(separator: " ")
        let why = String(String.UnicodeScalarView(collapsed.unicodeScalars.prefix(200)))
        let rid = kernel.mintId()
        try await saveHistory(
            key, kernel,
            [
                .object([
                    "role": .string("user"),
                    "id": .integer(rid),
                    "parent": .null,
                    "kind": .string("clear"),
                    "refs": .array(members),
                    "title": .string(why.isEmpty ? "cleared" : why),
                    "content": .string(
                        "[cleared \(rid) \u{b7} \(members.count) rows]"
                            + (why.isEmpty ? "" : " \(why)")
                            + " \u{2014} a new thread; the rows before it are this clear's members, "
                            + "reachable by id (grep, recall, history fold=\(rid))."),
                ])
            ])
        // The persisted view names rows behind the clear — empty it, so the view is
        // the store from the clear on. RAM is scratch; the store is never cleared.
        try await saveView(key, kernel, ids: [], budget: AIContext.budget(agent, modelWindow: config.modelWindow), why: "cleared")
        dropScratch(key, kernel)
        forgetClearState(key)
        return .object(["cleared": .integer(rid), "rows": .integer(Int64(members.count))])
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
    /// a build that raises (a record missing its endpoint or model) is `nil`: the
    /// recall then stays at the mechanical fit, exactly as python's
    /// `_get_provider` failure does.
    func sideProvider(agent: Agent, kernel: Kernel) async -> (any AIProvider)? {
        try? await config.makeProvider(agent, AIConstants.DEFAULT_CLIENT_ID, kernel)
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
    ) async throws -> JSON {
        let key = agent.id.value
        let rows = try await loadHistory(key, kernel)
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
            AIConstants.BUDGET_FLOOR, AIContext.budget(agent, modelWindow: config.modelWindow) - sysTokens - AIConstants.NOTICE_ENVELOPE_RESERVE)
        let target = Int64(Double(bodyBudget) * AIConstants.LOW)
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
        // PYTHON TRUTHINESS: `dry:1`, `"false"` and `[0]` are all dry.
        let dry = pyTruthy(payload["dry"])
        if let provider, !dry {
            var lines: [String] = []
            for i in head {
                let r = view[i]
                let content = rowContent(r)
                // `r.get("title") or …` — python TRUTHINESS and `str()`, so a
                // non-string title is used as written rather than skipped.
                let title =
                    pyTruthy(r["title"])
                    ? pyStr(r["title"])
                    : (AIFolds.quotedTitle(content)
                        ?? String(
                            String.UnicodeScalarView(
                                content.unicodeScalars.prefix(80).map { $0 == "\n" ? " " : $0 })))
                let kind = pyTruthy(r["kind"]) ? pyStr(r["kind"]) : pyStr(r["role"])
                let parent = pyTruthy(r["parent"]) ? pyStr(r["parent"]) : "-"
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
                // `_as_list(ans.get("expand"))` — a list as is, a scalar wrapped:
                // a list yields its items, a string its CODE POINTS, an object its
                // keys, and a truthy number or bool RAISES `'int' object is not
                // iterable` before anything is saved. Taking `asArray ?? []` made
                // every one of those a silent empty expansion, so a scalar
                // `expand` wrote a view and a recall row python never writes.
                let want: [Int64] = iterateExpand(ans["expand"] ?? .null)
                    .compactMap { aiAsId($0) }
                    .filter { headIds.contains($0) }
                // `str(ans.get("why") or "")` — 0, "", [] and {} are all falsy and
                // read `chosen`; anything truthy is its python `str`.
                let whyText = pyTruthy(ans["why"] ?? .null) ? pyStr(ans["why"] ?? .null) : ""
                let cutWhy = String(String.UnicodeScalarView(whyText.unicodeScalars.prefix(300)))
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
        try await saveView(key, kernel, ids: ids, budget: bodyBudget, why: why)
        // the decision is a ROW — the next reload reads why the last recall chose as it did
        let rid = kernel.mintId()
        // THE LIVE TASK IS NAMED. The view keeps the first root (the goal) at the
        // top, and after a recall a model with several roots re-answered the FIRST
        // one and called done (measured 2026-09-06, python fold_paging: TASK 0
        // re-answered at turn 88 with TASK 15 and a FINAL pending). The newest root
        // is what is live; say so. Over the STORE, not the folded render: a root
        // that a fold collapsed is still the live one if nothing newer came
        // (measured 2026-09-06, folds_x_chains: TASK C folded, the row named TASK A).
        let live = rows.last { r in
            // `not r.get("parent")` is python TRUTHINESS, and `kind in (None, "",
            // "user")` is EXACT membership — a row with `kind: 5` is not a root,
            // where reading `asString` and defaulting to true made it one.
            let kindIsRoot: Bool = {
                switch r["kind"] {
                case .null: return true
                case .string(let k): return k.isEmpty || k == "user"
                default: return false
                }
            }()
            return rowRole(r) == "user"
                && !pyTruthy(r["parent"])
                && kindIsRoot
                && parseReplyTag(rowContent(r)) == nil
        }
        let liveTxt: String =
            live.map { l in
                let collapsed = pySplitWhitespace(rowContent(l)).joined(separator: " ")
                let text = String(String.UnicodeScalarView(collapsed.unicodeScalars.prefix(100)))
                return " \u{b7} live task: \(rowId(l) ?? 0) \"\(text)\""
            } ?? ""
        try await saveHistory(
            key, kernel,
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
        dropScratch(key, kernel)
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

/// `for v in (value or [])` — python's iteration protocol over whatever the
/// model answered.
///
/// Python `_as_list(ans.get("expand"))` (2026-09-20): a list as is, a scalar
/// WRAPPED, a falsy value nothing. A truthy string or number used to be
/// iterated (characters, or a raise) — now it is one `_as_id` candidate.
private func iterateExpand(_ v: JSON) -> [JSON] {
    if !pyTruthy(v) { return [] }
    if case .array(let items) = v { return items }
    return [v]
}
