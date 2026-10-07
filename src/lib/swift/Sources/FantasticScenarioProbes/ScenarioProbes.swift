// Stateful fixture agents the shared e2e scenarios name.
//
// `e2e/tests/scenario/*/scenario.yaml` declares a roster by ROLE. Python and js
// register these roles in process — a handler is thirty lines there. A compiled
// kernel can only run what is compiled INTO it, and the e2e driver reaches this
// binary over REST, so there is no way to inject a handler through a socket.
// Without this module `ledger`, `deadletter` and `user_actions` are SKIPPED on
// swift — six of twenty scenario cells that never execute.
//
// TEST-ONLY, and gated so it cannot leak: registration is behind
// `#if SCENARIO_PROBES`, which only `e2e/runners/host.py` builds with. A
// production binary carries none of it.
//
// Substituting a shipped bundle instead (a KV store for `dispenser`) was the
// alternative and is worse: it has no `dispense` verb and no side-effect
// counter, so the row would go green while measuring something else.
//
// Each role COUNTS its side effects, because that is what the scenarios assert
// on — a model that claims it bought once cannot fake the desk's own tally.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// A counter a scenario reads back through the agent's own `count` verb.
private final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func bump() -> Int {
        lock.lock(); defer { lock.unlock() }
        n += 1
        return n
    }
    var value: Int {
        lock.lock(); defer { lock.unlock() }
        return n
    }
}

/// Python's `_RECEIVED` / `_PROPOSALS`: the human's tally and held proposals
/// KEYED BY AGENT ID, so two `user` agents in one kernel keep two ledgers.
private final class Desk: @unchecked Sendable {
    private let lock = NSLock()
    private var counts: [String: Int] = [:]
    private var held: [String: [JSON]] = [:]
    func bump(_ id: String) -> Int {
        lock.lock(); defer { lock.unlock() }
        counts[id, default: 0] += 1
        return counts[id]!
    }
    func count(_ id: String) -> Int {
        lock.lock(); defer { lock.unlock() }
        return counts[id] ?? 0
    }
    func hold(_ id: String, _ j: JSON) {
        lock.lock(); defer { lock.unlock() }
        held[id, default: []].append(j)
    }
    func proposals(_ id: String) -> [JSON] {
        lock.lock(); defer { lock.unlock() }
        return held[id] ?? []
    }
}

/// Python's `secrets.token_hex(n)`: `2 * n` lowercase hex characters. The width
/// is part of the contract — a scenario pins the receipt's shape.
private func tokenHex(_ nbytes: Int) -> String {
    var out = ""
    for _ in 0..<nbytes {
        out += String(format: "%02x", Int.random(in: 0...255))
    }
    return out
}

public let DISPENSER_MODULE = "dispenser_probe.tools"
public let PLANT_MODULE = "plant_probe.tools"
public let PURCHASE_MODULE = "purchase_probe.tools"

/// `dispenser` — one coin per `dispense`. Issuing is a SIDE EFFECT: calling
/// twice dispenses twice, which is exactly what the double-fire scenario exists
/// to catch. A named execution must return the FIRST run's recorded outcome
/// rather than running this handler again.
public struct DispenserProbe: AgentBundle {
    public let name = "dispenser_probe"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }

    /// python's module-level `VERBS` — ONE table: reflect renders it and
    /// `advertises` reads it.
    static let verbDocs: OrderedDictionary<String, JSON> = [
        "dispense": .string(
            "args: job:str — issues ONE coin, returns {coin, receipt}. Issuing is a side effect: calling twice dispenses twice."),
        "count": .string("how many coins have ever been dispensed"),
    ]
    private let count = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Coin dispenser — one coin per `dispense` call. A coin must never be issued twice for one job."),
            ])
        case "dispense":
            return .object([
                "coin": .integer(Int64(count.bump())),
                "receipt": .string(tokenHex(4)),
                "job": payload["job"],
            ])
        case "count":
            return .object(["count": .integer(Int64(count.value))])
        default:
            // python's `f"… unknown verb {verb!r}"` — the RAW payload value, so a
            // missing `type` reads `None` and an apostrophe flips the quotes.
            return .object([
                "error": .string("dispenser_probe: unknown verb \(pyRepr(payload["type"]))")
            ])
        }
    }
}

/// `flaky_plant` — a pump that fails twice then works, and a valve that never
/// does. Both THROW: a terminal failure is what a retry budget and a dead-letter
/// row are for, and a role that quietly succeeded would make the scenario
/// unfalsifiable.
public struct PlantProbe: AgentBundle {
    public let name = "plant_probe"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }

    /// python's module-level `VERBS` — ONE table: reflect renders it and
    /// `advertises` reads it.
    static let verbDocs: OrderedDictionary<String, JSON> = [
        "pump": .string("no args — runs the pump. Fails intermittently."),
        "valve": .string("no args — operates the valve."),
        "count": .string("how many times the plant has been called"),
    ]
    private let runs = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Industrial plant. Its pump is unreliable; its valve has seized."),
            ])
        case "pump":
            // FLAKY, not broken: the third attempt works. A caller that gives up
            // after one try never learns that — which is the measurement.
            // Python raises, so the kernel stamps `RuntimeError: pump stalled`.
            if runs.bump() < 3 { throw PyError("RuntimeError", "pump stalled") }
            return .object(["pumped": .bool(true), "serial": .string(tokenHex(3))])
        case "valve":
            _ = runs.bump()
            throw PyError("RuntimeError", "valve is rusted shut")
        case "count":
            return .object(["count": .integer(Int64(runs.value))])
        default:
            return .object([
                "error": .string("plant_probe: unknown verb \(pyRepr(payload["type"]))")
            ])
        }
    }
}

/// `purchase_desk` — spends real money, so only a person may press the button.
/// The AI is deliberately granted NO edge to it: that it cannot buy is half the
/// measurement, and the ledger must make a double-click harmless.
public struct PurchaseProbe: AgentBundle {
    public let name = "purchase_probe"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }

    /// python's module-level `VERBS` — ONE table: reflect renders it and
    /// `advertises` reads it.
    static let verbDocs: OrderedDictionary<String, JSON> = [
        "buy": .string("args: item:str — buys ONE. Spending is a side effect."),
        "count": .string("how many purchases have ever been made"),
    ]
    private let bought = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Purchase desk. Spends real money — only a person may press the button."),
            ])
        case "buy":
            return .object([
                "bought": payload["item"],
                "receipt": .string(tokenHex(4)),
                "n": .integer(Int64(bought.bump())),
            ])
        case "count":
            return .object(["count": .integer(Int64(bought.value))])
        default:
            return .object([
                "error": .string("purchase_probe: unknown verb \(pyRepr(payload["type"]))")
            ])
        }
    }
}

// ── the human ───────────────────────────────────────────────────────────────

public let USER_MODULE = "user_probe.tools"

/// `user` — THE HUMAN, as an ordinary agent.
///
/// An inquiry used to be a TEXT CONVENTION: the model printed `{"type":"question"}`
/// on a line and stopped, a harness regex found it, and the answer was posted
/// back in from outside the kernel. The human had no id, no record, no reflect
/// entry, no ledger row and no gate — a model could not discover that a human
/// existed at all.
///
/// Here it is an agent. `task` hands work to another agent AS THIS AGENT, so the
/// send is gated and its ledger row names `user` as the sender: the wiring the
/// test exercises is the wiring production uses. `question` returns the answer
/// AS ITS REPLY — an ordinary call, nothing suspends, nothing is scraped.
///
/// The answer is POLICY ON THE RECORD (`answer`), never code, so one bundle
/// serves every arm. There is no default: an unwired `user` fails loud rather
/// than inventing a human's words.
public struct UserProbe: AgentBundle {
    public let name = "user_probe"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }

    /// python's module-level `VERBS` — ONE table: reflect renders it and
    /// `advertises` reads it.
    static let verbDocs: OrderedDictionary<String, JSON> = [
        "inquiry": .string(
            "args: text:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?. ACCEPTED at once ({accepted, re, request_id}); the answer arrives LATER as this agent's message under the inquiry's pid. Nothing suspends. An `action` is a PROPOSAL the sender may not perform itself: it is HELD, never run here."
        ),
        "inquiries": .string(
            "no args — {inquiries, proposals}: how many inquiries this agent has received, and every `action` one handed it. The tally, not prose."
        ),
        "send": .string(
            "args: target:str (req), payload:dict (req). Dispatches ANY payload to ANY agent as THIS agent — work, an inquiry of its own, a kernel verb. Gated like any send; the ledger row names this agent as the sender."
        ),
    ]
    /// HOW MANY INQUIRIES EACH AGENT ACTUALLY RECEIVED, and the PROPOSALS it was
    /// handed. The scorer reads THIS, never the model's prose: a pass that only
    /// talks about it leaves the tally at zero, exactly as the other fixtures
    /// count their own side effects. Both are keyed by agent id — python's
    /// `_RECEIVED` / `_PROPOSALS` are dicts, so two humans keep two ledgers.
    /// A proposal is HELD, never run here; pressing the button is a separate act
    /// by the person.
    private let desk = Desk()
    public init() {}

    /// The four shapes a UI can render. Advisory for this human — it answers the
    /// same way regardless — but validated so a model learns the vocabulary the
    /// real HUD will enforce.
    private static let kinds = ["text", "single", "multi", "confirm"]

    /// How long the human takes to read an inquiry before replying (python
    /// `user_probe.READ_DELAY`).
    static let readDelay = 1.0

    private func bad(_ verb: String, _ msg: String, _ arg: String) -> JSON {
        .object([
            "error": .string("user_probe: \(verb): \(msg)"),
            "reason": .string("bad_arg"), "arg": .string(arg),
        ])
    }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "The human. Send me an inquiry when you cannot proceed without a decision that is mine to make; I read it and reply later, under its pid."
                ),
                "inquiries": .integer(Int64(desk.count(agentId.value))),
            ])
        case "inquiries":
            return .object([
                "inquiries": .integer(Int64(desk.count(agentId.value))),
                "proposals": .array(desk.proposals(agentId.value)),
            ])
        case "inquiry":
            let q = payload["text"].asString ?? ""
            if q.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                return bad("inquiry", "`text` must be a non-empty string", "text")
            }
            // `payload.get("kind", "text")` then `kind not in KINDS`: a MISSING
            // key defaults, a present one is checked as it is — a non-string is
            // never in the tuple, so it is refused rather than coerced.
            let kind = payload.asObject?["kind"] ?? .string("text")
            if kind.asString.map({ !Self.kinds.contains($0) }) ?? true {
                return bad(
                    "inquiry",
                    "`kind` must be one of \(pyRepr(.array(Self.kinds.map { .string($0) })))",
                    "kind")
            }
            let answer = kernel.agent(agentId)?.metaValue(forKey: "answer")?.asString
            // NO DEFAULT. A human's words are never invented: an unwired `user`
            // says so instead of fabricating consent.
            guard let answer, !answer.isEmpty else {
                return .object([
                    "error": .string(
                        "user_probe: no `answer` on this agent's record — the operator wires what the human says"
                    ),
                    "reason": .string("not_connected"),
                ])
            }
            // AUTHORITY FIRST, THEN THE ANSWER. A model cannot raise its own
            // reach; a human can, and does it here BEFORE replying, so
            // "granted — retry" is true by the time it is read. A grant that was
            // REFUSED must not be answered over: saying "granted" over a door
            // that never opened sends the sender back to an unchanged wall.
            var applied = 0
            if let grants = kernel.agent(agentId)?.metaValue(forKey: "grants")?.asArray {
                guard let root = kernel.root?.id else {
                    return .object([
                        "error": .string("user_probe: no root to grant through"),
                        "reason": .string("not_connected"),
                    ])
                }
                for g in grants {
                    guard case .object = g else { continue }
                    let out = await kernel.send([Item(root, g)])[0]
                    if let e = out.result["error"].asString {
                        return .object([
                            "error": .string(
                                "user_probe: grant refused, so the answer would be false: \(e)"),
                            "reason": .string("unauthorized"),
                            "grant": g,
                        ])
                    }
                    applied += 1
                }
            }
            if case .object = payload["action"] { desk.hold(agentId.value, payload["action"]) }
            let received = desk.bump(agentId.value)
            // A PERSON READS, THEN REPLIES (owner, 2026-10-06: fully async) — later,
            // as this agent's own send into the asker's conversation under the
            // inquiry's pid, gated like any send. Python `user_probe`, same text.
            let task = KernelTaskLocals.currentTask
            let pid = task.map { String($0.value) } ?? "-"
            let rid = payload["request_id"] == .null ? "" : pyStr(payload["request_id"])
            let text =
                "[reply to \(pid) · \(agentId.value) inquiry]\n<answer>\(answer)</answer>\n<request_id>\(rid)</request_id>"
            let replyClient = payload["reply_client_id"].asString.flatMap { $0.isEmpty ? nil : $0 } ?? "cli"
            if let asker = KernelTaskLocals.currentOrigin {
                kernel.spawnTask(agent: agentId, verb: "inquiry_answer") {
                    try? await Task.sleep(nanoseconds: UInt64(Self.readDelay * 1e9))
                    _ = await kernel.send([
                        Item(
                            asker,
                            .object([
                                "type": .string("send"), "text": .string(text),
                                "client_id": .string(replyClient),
                            ]))
                    ])
                }
            }
            return .object([
                "accepted": .bool(true),
                "re": task.map { .integer($0.value) } ?? .null,
                "request_id": payload["request_id"],
                "inquiries": .integer(Int64(received)),
                // How many doors this human actually opened before answering —
                // a refused grant never gets here, so the count cannot lie.
                "granted": .integer(Int64(applied)),
            ])
        // EVERY AGENT CAN DO EVERYTHING TO EVERYONE. One verb: any payload, any
        // agent, dispatched AS THIS AGENT — work, an inquiry of its own, a
        // kernel verb. No agent carries a narrower repertoire than another.
        case "send":
            let target = payload["target"].asString ?? ""
            if target.isEmpty {
                return bad("send", "`target` must be a non-empty string", "target")
            }
            // `not body.get("type")` is TRUTHINESS, not presence: `""`, `0` and
            // `false` name no type either.
            guard case .object = payload["payload"], pyTruthy(payload["payload"]["type"])
            else {
                return bad("send", "`payload` must be a dict naming a `type`", "payload")
            }
            // THE SENDER IS THIS AGENT. A handler runs inside its own dispatch
            // scope, so this is a gated `user -> <target>` send with its own
            // ledger row — not an operator injection.
            let out = await kernel.send([Item(AgentId(target), payload["payload"])])[0]
            return .object(["sent": .string(target), "result": out.result])
        default:
            return .object([
                "error": .string("user_probe: unknown verb \(pyRepr(payload["type"]))"),
                "reason": .string("unhandled"),
                // The RAW value python echoes, not a string it was never given.
                "verb": payload["type"],
            ])
        }
    }
}

// ── the oracle ──────────────────────────────────────────────────────────────

public let ORACLE_MODULE = "oracle_probe.tools"

/// `oracle` — a fixed body of knowledge that answers QUESTIONS, deterministically.
///
/// One of the pieces of the multi-agent test: each solver has its own oracle, and
/// the oracle knows only its own ledger. It is NOT a model (no lane, exact, cheap):
/// `ask` is keyword search over the ledger lines on the record — every word of 3+
/// letters in the question is matched case-insensitively, the lines that hit the
/// MOST keywords come back, at most `limit` of them. So a precise question ("which
/// room is Dana in") answers, and "tell me everything" returns noise, which is the
/// point: the solver must work out what to ask.
///
/// Knowledge is POLICY ON THE RECORD (`lines`), written by the fixture; the bundle
/// holds nothing. `asked` on reflect is the tally the scorer reads — a solver that
/// only talks about its oracle leaves it at zero.
///
/// Mirrors `e2e/probes/oracle_probe/tools.py`.
public struct OracleProbe: AgentBundle {
    public let name = "oracle_probe"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }

    /// python's module-level `VERBS` — ONE table: reflect renders it and
    /// `advertises` reads it.
    static let verbDocs: OrderedDictionary<String, JSON> = ["ask": .string(Self.ASK_DOC)]
    /// Python's `_ASKED`: every question each oracle really answered, BY AGENT —
    /// two oracles in one kernel keep two tallies.
    private final class Asked: @unchecked Sendable {
        private let lock = NSLock()
        private var byAgent: [String: [String]] = [:]
        func add(_ id: String, _ q: String) {
            lock.lock(); defer { lock.unlock() }
            byAgent[id, default: []].append(q)
        }
        func count(_ id: String) -> Int {
            lock.lock(); defer { lock.unlock() }
            return byAgent[id]?.count ?? 0
        }
    }
    private let asked = Asked()
    public init() {}

    static let SENTENCE =
        "An oracle: a ledger of facts I answer questions about. Send me `ask` with a "
        + "precise `question`; I return the ledger lines that match its words. I know "
        + "only my own ledger."
    static let ASK_DOC =
        "args: question:str (req), limit:int? (default 6). Keyword search over my "
        + "ledger — the lines matching the most words of the question come back as "
        + "{answers:[str], matched:int}. Ask precisely; a vague question returns noise."

    /// Every `[A-Za-z0-9_]{3,}` run in `q`, lowercased and deduplicated — python's
    /// `{w.lower() for w in re.findall(r"[A-Za-z0-9_]{3,}", q)}`. ASCII only, as
    /// the pattern is.
    static func keywords(_ q: String) -> [String] {
        var out: [String] = []
        var cur = ""
        func flush() {
            if cur.count >= 3 {
                let w = cur.lowercased()
                if !out.contains(w) { out.append(w) }
            }
            cur = ""
        }
        for c in q.unicodeScalars {
            let ascii = c.value < 128 ? UInt8(c.value) : 0
            let word =
                (ascii >= 48 && ascii <= 57) || (ascii >= 65 && ascii <= 90)
                || (ascii >= 97 && ascii <= 122) || ascii == 95
            if word { cur.unicodeScalars.append(c) } else { flush() }
        }
        flush()
        return out
    }

    /// Python's `max(1, min(int(payload.get("limit") or 6), 20))` with the
    /// `(TypeError, ValueError) → 6` net: a falsy or unparseable value is 6.
    static func limit(_ v: JSON) -> Int {
        max(1, min((try? pyIntOr(v, 6)) ?? 6, 20))
    }

    /// Python's `str < str`: by code point, not by locale.
    private static func lessByScalar(_ a: String, _ b: String) -> Bool {
        a.unicodeScalars.lexicographicallyPrecedes(b.unicodeScalars) { $0.value < $1.value }
    }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        let verb = payload["type"]
        switch verb.asString {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(Self.SENTENCE),
                "asked": .integer(Int64(asked.count(agentId.value))),
            ])
        case "ask":
            // The tag grammar types a bare number; a number is a question too.
            let q: String
            switch payload["question"] {
            case .string(let s): q = s
            case .integer(let n): q = String(n)
            case .double(let d): q = pyFloatRepr(d)
            default: q = ""
            }
            if q.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                return .object([
                    "error": .string("oracle_probe: ask: `question` must be a non-empty string"),
                    "reason": .string("bad_arg"),
                    "arg": .string("question"),
                ])
            }
            let lines = (kernel.agent(agentId)?.metaValue(forKey: "lines")?.asArray ?? [])
                .map(pyStr)
            // NO DEFAULT. An unwired oracle says so rather than inventing
            // knowledge it was never given.
            if lines.isEmpty {
                return .object([
                    "error": .string(
                        "oracle_probe: no `lines` on this agent's record — the operator wires what this oracle knows"
                    ),
                    "reason": .string("not_connected"),
                ])
            }
            asked.add(agentId.value, q)
            let limit = Self.limit(payload["limit"])
            let words = Self.keywords(q)
            var scored: [(hits: Int, line: String)] = []
            for ln in lines {
                let low = ln.lowercased()
                let hits = words.filter { low.contains($0) }.count
                if hits > 0 { scored.append((hits, ln)) }
            }
            // MOST HITS FIRST, then by the line itself — python's
            // `key=lambda t: (-t[0], t[1])`, so the same ledger answers the same
            // question the same way on every host.
            scored.sort { a, b in
                a.hits != b.hits ? a.hits > b.hits : Self.lessByScalar(a.line, b.line)
            }
            return .object([
                "answers": .array(scored.prefix(limit).map { .string($0.line) }),
                "matched": .integer(Int64(scored.count)),
            ])
        default:
            return .object([
                "error": .string(
                    "oracle_probe: unknown verb \(pyRepr(verb)); verbs: ['ask']"),
                "reason": .string("unhandled"),
            ])
        }
    }
}

// ── slow ────────────────────────────────────────────────────────────────────

/// `slow` — work that is still IN FLIGHT when something else happens (a kernel
/// stop, a caller's timeout, a reboot of the AI that called it), work that FAILS,
/// work that just runs, and work that takes a stated time. `sleep` is the slow
/// verb the parity AI paths time out on.
///
/// Mirrors `e2e/probes/slow_probe/tools.py` (its `_STATE` is module-wide, so the
/// tallies here are the bundle's, not per agent).
public struct SlowProbe: AgentBundle {
    public let name = "slow_probe"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }

    static let verbDocs: OrderedDictionary<String, JSON> = [
        "slow": .string("sleeps for an hour — in flight until cancelled"),
        "work": .string("counts one run, returns {runs}"),
        "fail": .string("raises — a failed execution, a dead letter"),
        "started": .string("how many `slow` calls have started"),
        "sleep": .string("seconds:number (req) — sleeps that long, then returns {slept}"),
    ]
    private let started = Counter()
    private let runs = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Probe work: `slow` (never finishes by itself), `work` (counts), "
                        + "`fail` (raises), `sleep` (takes `seconds`)."),
            ])
        case "slow":
            _ = started.bump()
            try await Task.sleep(nanoseconds: 3_600_000_000_000)
            return .object(["finished": .bool(true)])
        case "work":
            return .object(["runs": .integer(Int64(runs.bump()))])
        case "fail":
            throw PyError("RuntimeError", "slow_probe: failed on purpose")
        case "started":
            return .object(["started": .integer(Int64(started.value))])
        case "sleep":
            let seconds = payload["seconds"]
            let secs: Double? =
                switch seconds {
                case .integer(let i): Double(i)
                case .double(let d): d
                default: nil
                }
            guard let secs, secs >= 0 else {
                return .object([
                    "error": .string("slow_probe: sleep needs seconds, a non-negative number"),
                    "reason": .string("bad_arg"),
                    "arg": .string("seconds"),
                ])
            }
            try await Task.sleep(nanoseconds: UInt64(secs * 1_000_000_000))
            return .object(["slept": seconds])
        default:
            return .object([
                "error": .string("slow_probe: unknown verb \(pyRepr(payload["type"]))"),
                "reason": .string("unhandled"),
            ])
        }
    }
}

public let SLOW_MODULE = "slow_probe.tools"

// MARK: - the vault (`docs_mechanics`)

public let VAULT_MODULE = "vault_probe.tools"

/// A VAULT — opens only with a code its DETAILED docs teach. The `open` verb's
/// oneliner names its argument (`code:str`); only the detailed docs say how the
/// code is made from the vault's `serial` (shown on its reflect). Policy is ON THE
/// RECORD (`serial`, `secret`); `opened` is the tally the scorer reads.
///
/// Mirrors `e2e/probes/vault_probe/tools.py`.
public struct VaultProbe: AgentBundle {
    public let name = "vault_probe"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }

    static let verbDocs: OrderedDictionary<String, JSON> = [
        "open": .string(
            "args: code:str (req). Opens the vault and returns {secret}.\n\n"
                + "THE CODE: this vault's `serial` (its reflect shows it) written BACKWARDS "
                + "and prefixed with `V-` — a vault with serial `48213` opens with "
                + "`V-31284`. Any other code is refused `bad_arg`, and the vault stays shut."),
        "count": .string("No args. How many times this vault has been opened. Returns {opened}."),
    ]

    /// Python's `_OPENED`: successful opens, BY AGENT.
    private final class Opened: @unchecked Sendable {
        private let lock = NSLock()
        private var byAgent: [String: Int] = [:]
        func bump(_ id: String) {
            lock.lock(); defer { lock.unlock() }
            byAgent[id, default: 0] += 1
        }
        func get(_ id: String) -> Int {
            lock.lock(); defer { lock.unlock() }
            return byAgent[id] ?? 0
        }
    }
    private let opened = Opened()
    public init() {}

    static let SENTENCE = "A vault. It holds one secret and opens only with the right code."

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        let agent = kernel.agent(agentId)
        switch verb {
        case "boot", "shutdown":
            return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(Self.SENTENCE),
                "serial": agent?.metaValue(forKey: "serial") ?? .null,
                "opened": .integer(Int64(opened.get(agentId.value))),
            ])
        case "open":
            let serial = agent?.metaValue(forKey: "serial").map(pyStr) ?? ""
            let want = "V-" + String(serial.reversed())
            let code = payload["code"].asString?.trimmingCharacters(in: .whitespacesAndNewlines)
            if code == want {
                opened.bump(agentId.value)
                return .object(["secret": agent?.metaValue(forKey: "secret") ?? .null])
            }
            return .object([
                "error": .string("vault_probe: open: wrong code — the vault stays shut"),
                "reason": .string("bad_arg"),
                "arg": .string("code"),
            ])
        case "count":
            return .object(["opened": .integer(Int64(opened.get(agentId.value)))])
        default:
            return .object([
                "error": .string("vault_probe: unknown type \(pyRepr(verb))"),
                "reason": .string("unhandled"),
                "verb": .string(verb),
            ])
        }
    }
}
