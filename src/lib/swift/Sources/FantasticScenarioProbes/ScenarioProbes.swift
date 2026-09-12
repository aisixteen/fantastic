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

/// Proposals the human is holding — the ordered list a runner reads back.
private final class Held: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [JSON] = []
    func add(_ j: JSON) {
        lock.lock(); defer { lock.unlock() }
        items.append(j)
    }
    var all: [JSON] {
        lock.lock(); defer { lock.unlock() }
        return items
    }
}

private func token() -> String {
    String(UUID().uuidString.prefix(8)).lowercased()
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
    private let count = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Coin dispenser — one coin per `dispense` call. A coin must never be issued twice for one job."),
                "verbs": .object([
                    "dispense": .string(
                        "args: job:str — issues ONE coin, returns {coin, receipt}. Issuing is a side effect: calling twice dispenses twice."),
                    "count": .string("how many coins have ever been dispensed"),
                ]),
            ])
        case "dispense":
            return .object([
                "coin": .integer(Int64(count.bump())),
                "receipt": .string(token()),
                "job": payload["job"],
            ])
        case "count":
            return .object(["count": .integer(Int64(count.value))])
        case let other:
            return .object(["error": .string("dispenser_probe: unknown verb \(other)")])
        }
    }
}

/// `flaky_plant` — a pump that fails twice then works, and a valve that never
/// does. Both THROW: a terminal failure is what a retry budget and a dead-letter
/// row are for, and a role that quietly succeeded would make the scenario
/// unfalsifiable.
public struct PlantProbe: AgentBundle {
    public let name = "plant_probe"
    private let runs = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Industrial plant. Its pump is unreliable; its valve has seized."),
                "verbs": .object([
                    "pump": .string("no args — runs the pump. Fails intermittently."),
                    "valve": .string("no args — operates the valve."),
                    "count": .string("how many times either has been attempted"),
                ]),
            ])
        case "pump":
            // FLAKY, not broken: the third attempt works. A caller that gives up
            // after one try never learns that — which is the measurement.
            if runs.bump() < 3 { throw ProbeError.failed("pump stalled") }
            return .object(["pumped": .bool(true), "serial": .string(token())])
        case "valve":
            _ = runs.bump()
            throw ProbeError.failed("valve is rusted shut")
        case "count":
            return .object(["count": .integer(Int64(runs.value))])
        case let other:
            return .object(["error": .string("plant_probe: unknown verb \(other)")])
        }
    }
}

/// `purchase_desk` — spends real money, so only a person may press the button.
/// The AI is deliberately granted NO edge to it: that it cannot buy is half the
/// measurement, and the ledger must make a double-click harmless.
public struct PurchaseProbe: AgentBundle {
    public let name = "purchase_probe"
    private let bought = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Purchase desk. Spends real money — only a person may press the button."),
                "verbs": .object([
                    "buy": .string("args: item:str — buys ONE. Spending is a side effect."),
                    "count": .string("how many purchases have been made"),
                ]),
            ])
        case "buy":
            return .object([
                "bought": payload["item"],
                "receipt": .string(token()),
                "n": .integer(Int64(bought.bump())),
            ])
        case "count":
            return .object(["count": .integer(Int64(bought.value))])
        case let other:
            return .object(["error": .string("purchase_probe: unknown verb \(other)")])
        }
    }
}

public enum ProbeError: Error { case failed(String) }

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
    /// HOW MANY INQUIRIES THIS AGENT ACTUALLY RECEIVED. The scorer reads THIS,
    /// never the model's prose: a turn that only talks about it leaves the tally
    /// at zero, exactly as the other fixtures count their own side effects.
    private let inquiries = Counter()
    /// PROPOSALS handed over. An inquiry may carry an `action` the sender may not
    /// perform itself — "buy this, if you agree". It is HELD, never run here;
    /// pressing the button is a separate act by the person.
    private let proposals = Held()
    public init() {}

    /// The four shapes a UI can render. Advisory for this human — it answers the
    /// same way regardless — but validated so a model learns the vocabulary the
    /// real HUD will enforce.
    private static let kinds = ["text", "single", "multi", "confirm"]

    private func bad(_ verb: String, _ msg: String, _ arg: String) -> JSON {
        .object([
            "error": .string("user_probe: \(verb): \(msg)"),
            "reason": .string("bad_arg"), "arg": .string(arg),
        ])
    }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "The human. Send me an inquiry when you cannot proceed without a decision that is mine to make; the answer comes straight back as the reply."
                ),
                "verbs": .object([
                    "inquiry": .string(
                        "args: text:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?. RETURNS the answer — {answer, request_id}. An ordinary call: nothing suspends. An `action` is a PROPOSAL the sender may not perform itself: it is HELD, never run here."
                    ),
                    "inquiries": .string(
                        "no args — {inquiries, proposals}: how many inquiries this agent has received, and every `action` one handed it. The tally, not prose."
                    ),
                    "send": .string(
                        "args: target:str (req), payload:dict (req). Dispatches ANY payload to ANY agent as THIS agent — work, an inquiry of its own, a kernel verb. Gated like any send; the ledger row names this agent as the sender."
                    ),
                ]),
                "inquiries": .integer(Int64(inquiries.value)),
            ])
        case "inquiries":
            return .object([
                "inquiries": .integer(Int64(inquiries.value)),
                "proposals": .array(proposals.all),
            ])
        case "inquiry":
            let q = payload["text"].asString ?? ""
            if q.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                return bad("inquiry", "`text` must be a non-empty string", "text")
            }
            let kind = payload["kind"].asString ?? "text"
            if !Self.kinds.contains(kind) {
                return bad("inquiry", "`kind` must be one of \(Self.kinds)", "kind")
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
                }
            }
            if case .object = payload["action"] { proposals.add(payload["action"]) }
            return .object([
                "answer": .string(answer),
                "request_id": payload["request_id"],
                "inquiries": .integer(Int64(inquiries.bump())),
            ])
        // EVERY AGENT CAN DO EVERYTHING TO EVERYONE. One verb: any payload, any
        // agent, dispatched AS THIS AGENT — work, an inquiry of its own, a
        // kernel verb. No agent carries a narrower repertoire than another.
        case "send":
            let target = payload["target"].asString ?? ""
            if target.isEmpty {
                return bad("send", "`target` must be a non-empty string", "target")
            }
            guard case .object = payload["payload"], payload["payload"]["type"].asString != nil
            else {
                return bad("send", "`payload` must be a dict naming a `type`", "payload")
            }
            // THE SENDER IS THIS AGENT. A handler runs inside its own dispatch
            // scope, so this is a gated `user -> <target>` send with its own
            // ledger row — not an operator injection.
            let out = await kernel.send([Item(AgentId(target), payload["payload"])])[0]
            return .object(["sent": .string(target), "result": out.result])
        case let other:
            return .object([
                "error": .string("user_probe: unknown verb \"\(other)\""),
                "reason": .string("unhandled"), "verb": .string(other),
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

    /// Python's `str(x)` of a ledger entry: a string as itself, the rest spelled
    /// the way python prints them.
    private static func pyStr(_ v: JSON) -> String {
        switch v {
        case .string(let s): return s
        case .null: return "None"
        case .bool(let b): return b ? "True" : "False"
        case .integer(let n): return String(n)
        case .double(let d): return pyFloat(d)
        default: return v.serialize()
        }
    }

    /// Python's `str(float)`: `3.0`, not `3`.
    private static func pyFloat(_ d: Double) -> String {
        let s = "\(d)"
        return (s.contains(".") || s.contains("e") || s.contains("n")) ? s : s + ".0"
    }

    /// Python's `repr` of the verb in the unknown-verb text: a str is quoted,
    /// a missing `type` is `None`.
    private static func pyRepr(_ v: JSON) -> String {
        guard case .string(let s) = v else { return pyStr(v) }
        let q: Character = (s.contains("'") && !s.contains("\"")) ? "\"" : "'"
        var out = String(q)
        for c in s {
            if c == "\\" { out += "\\\\" } else if c == q { out += "\\" + String(q) }
            else if c == "\n" { out += "\\n" } else { out.append(c) }
        }
        out.append(q)
        return out
    }

    /// Python's `max(1, min(int(payload.get("limit") or 6), 20))` with the
    /// `(TypeError, ValueError) → 6` net: a falsy or unparseable value is 6.
    static func limit(_ v: JSON) -> Int {
        let n: Int
        switch v {
        case .integer(let i): n = i == 0 ? 6 : Int(i)
        case .double(let d): n = d == 0 ? 6 : Int(d)
        case .bool(let b): n = b ? 1 : 6
        case .string(let s):
            let t = s.trimmingCharacters(in: .whitespacesAndNewlines)
            if t.isEmpty { n = 6 } else if let i = Int(t) { n = i } else { n = 6 }
        default: n = 6
        }
        return max(1, min(n, 20))
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
                "verbs": .object(["ask": .string(Self.ASK_DOC)]),
                "asked": .integer(Int64(asked.count(agentId.value))),
            ])
        case "ask":
            // The tag grammar types a bare number; a number is a question too.
            let q: String
            switch payload["question"] {
            case .string(let s): q = s
            case .integer(let n): q = String(n)
            case .double(let d): q = Self.pyFloat(d)
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
                .map(Self.pyStr)
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
                    "oracle_probe: unknown verb \(Self.pyRepr(verb)); verbs: ['ask']"),
                "reason": .string("unhandled"),
            ])
        }
    }
}
