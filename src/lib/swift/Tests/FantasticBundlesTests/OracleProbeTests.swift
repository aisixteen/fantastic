// oracle_probe — the fixed-knowledge fixture the multi-agent scenarios name.
// Every text here is python's (`e2e/probes/oracle_probe/tools.py`), pinned
// verbatim: the scorer and the solver's prompt read these bytes on every host.

import FantasticJSON
import FantasticKernel
@testable import FantasticScenarioProbes
import Foundation
import Testing

private func oracleKernel(lines: JSON?) async -> (Kernel, AgentId) {
    let r = BundleRegistry()
    r.register(ORACLE_MODULE, OracleProbe())
    let kernel = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    var rec: JSON = [
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": .string(ORACLE_MODULE), "id": "oracle",
    ]
    if let lines { rec["lines"] = lines }
    let out = await kernel.send([Item("kernel_state", rec)])[0].result
    #expect(out["id"].asString == "oracle", "\(out)")
    return (kernel, AgentId("oracle"))
}

@Suite("oracle_probe")
struct OracleProbeTests {
    private let ledger: JSON = [
        "Dana is in room 4.",
        "Room 4 has the blue key.",
        "Bob is in room 2.",
        "The key opens the DANA vault.",
    ]

    @Test func reflectIsPythonsShapeAndText() async {
        let (kernel, id) = await oracleKernel(lines: ledger)
        let r = await kernel.send([Item(id, ["type": "reflect"])])[0].result
        #expect(r["id"].asString == "oracle")
        #expect(
            r["sentence"].asString
                == "An oracle: a ledger of facts I answer questions about. Send me `ask` with a precise `question`; I return the ledger lines that match its words. I know only my own ledger."
        )
        let d = OracleProbe().verbDocTable
        #expect(
            d["ask"].asString
                == "args: question:str (req), limit:int? (default 6). Keyword search over my ledger — the lines matching the most words of the question come back as {answers:[str], matched:int}. Ask precisely; a vague question returns noise."
        )
        #expect(d.asObject?.count == 1)
        #expect(r["asked"].asInt == 0)
        // The bundle's own fields, in python's order; the substrate appends its frame after them.
        #expect(r.asObject?.keys.starts(with: ["id", "sentence", "asked"]) == true, "\(r)")
    }

    @Test func askRanksByHitsThenLineAndTalliesPerAgent() async {
        let (kernel, id) = await oracleKernel(lines: ledger)
        // "which room is Dana in" → keywords {which, room, dana}; two lines hit
        // twice, ordered by the line text; `is`/`in` are shorter than 3.
        let r = await kernel.send([Item(id, ["type": "ask", "question": "which room is Dana in"])])[0].result
        #expect(r["matched"].asInt == 4, "\(r)")
        #expect(
            r["answers"].asArray?.map { $0.asString } == [
                "Dana is in room 4.", "Bob is in room 2.", "Room 4 has the blue key.",
                "The key opens the DANA vault.",
            ], "\(r)")
        // `limit` clamps into [1, 20]; a bad value is 6.
        let one = await kernel.send([Item(id, ["type": "ask", "question": "room", "limit": 1])])[0].result
        #expect(one["answers"].asArray?.count == 1 && one["matched"].asInt == 3)
        let bad = await kernel.send([Item(id, ["type": "ask", "question": "room", "limit": "many"])])[0].result
        #expect(bad["answers"].asArray?.count == 3)
        let zero = await kernel.send([Item(id, ["type": "ask", "question": "room", "limit": 0])])[0].result
        #expect(zero["answers"].asArray?.count == 3)
        // A bare number is a question too (the tag grammar types it).
        let num = await kernel.send([Item(id, ["type": "ask", "question": 404])])[0].result
        #expect(num["matched"].asInt == 0 && num["answers"].asArray?.isEmpty == true)
        // The tally counts every answered ask on THIS agent.
        let r2 = await kernel.send([Item(id, ["type": "reflect"])])[0].result
        #expect(r2["asked"].asInt == 5)
    }

    @Test func refusalsArePythonsBytes() async {
        let (kernel, id) = await oracleKernel(lines: ledger)
        let empty = await kernel.send([Item(id, ["type": "ask", "question": "   "])])[0].result
        #expect(empty["error"].asString == "oracle_probe: ask: `question` must be a non-empty string")
        #expect(empty["reason"].asString == "bad_arg" && empty["arg"].asString == "question")
        let missing = await kernel.send([Item(id, ["type": "ask"])])[0].result
        #expect(missing["reason"].asString == "bad_arg")
        let zap = await kernel.send([Item(id, ["type": "zap"])])[0].result
        #expect(zap["error"].asString == "oracle_probe: unknown verb 'zap'; verbs: ['ask']")
        #expect(zap["reason"].asString == "unhandled")
        // A refused ask is not tallied.
        let r = await kernel.send([Item(id, ["type": "reflect"])])[0].result
        #expect(r["asked"].asInt == 0)
    }

    @Test func unwiredOracleSaysSo() async {
        let (kernel, id) = await oracleKernel(lines: nil)
        let r = await kernel.send([Item(id, ["type": "ask", "question": "anything at all"])])[0].result
        #expect(
            r["error"].asString
                == "oracle_probe: no `lines` on this agent's record — the operator wires what this oracle knows")
        #expect(r["reason"].asString == "not_connected")
        let (k2, id2) = await oracleKernel(lines: [])
        let r2 = await k2.send([Item(id2, ["type": "ask", "question": "anything at all"])])[0].result
        #expect(r2["reason"].asString == "not_connected")
    }

    @Test func keywordsAreThreePlusAsciiWordCharsLowercased() {
        #expect(OracleProbe.keywords("Which ROOM is Dana_1 in? café") == ["which", "room", "dana_1", "caf"])
        #expect(OracleProbe.limit(.null) == 6)
        #expect(OracleProbe.limit(.integer(99)) == 20)
        #expect(OracleProbe.limit(.integer(-3)) == 1)
        #expect(OracleProbe.limit(.string(" 4 ")) == 4)
        #expect(OracleProbe.limit(.double(2.9)) == 2)
    }

    @Test func oracleLimitCoercesLikePython() {
        // `max(1, min(int(payload.get("limit") or 6), 20))` with python's
        // `(TypeError, ValueError) → 6` net around it — falsy is the default,
        // unparseable is the default, and the clamp is inclusive both ends.
        #expect(OracleProbe.limit(.string("3")) == 3)
        #expect(OracleProbe.limit(.integer(0)) == 6)
        #expect(OracleProbe.limit(.string("x")) == 6)
        #expect(OracleProbe.limit(.integer(99)) == 20)
        #expect(OracleProbe.limit(.double(2.9)) == 2)
        #expect(OracleProbe.limit(.bool(true)) == 1)
        #expect(OracleProbe.limit(.integer(-5)) == 1)
    }

    @Test func oracleRendersNonStringLinesWithPythonStr() async {
        // `str(ln)` on every ledger line, for the match AND for the answer: a
        // JSON dump would spell `True` as `true` and a dict without its quotes.
        let (kernel, id) = await oracleKernel(lines: [700, true, .null, ["abc": 1]])
        let r = await kernel.send([
            Item(id, ["type": "ask", "question": "700 true none abc"])
        ])[0].result
        #expect(r["matched"].asInt == 4, "\(r)")
        // One hit each, so the tie breaks on the line itself, by code point.
        #expect(
            (r["answers"].asArray ?? []).compactMap { $0.asString }
                == ["700", "None", "True", "{'abc': 1}"], "\(r)")
    }

    @Test func twoOraclesKeepTwoTallies() async {
        // Python's `_ASKED` is a dict keyed by agent id: each solver's oracle
        // counts only its own questions, which is what the scorer reads.
        let r = BundleRegistry()
        r.register(ORACLE_MODULE, OracleProbe())
        let kernel = Kernel(storage: .inMemory, bundles: r)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        for name in ["o1", "o2"] {
            let rec: JSON = [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": .string(ORACLE_MODULE), "id": .string(name),
                "lines": ["Dana is in room 4."],
            ]
            let out = await kernel.send([Item("kernel_state", rec)])[0].result
            #expect(out["id"].asString == name, "\(out)")
        }
        for _ in 0..<3 {
            _ = await kernel.send([Item("o1", ["type": "ask", "question": "which room"])])[0].result
        }
        _ = await kernel.send([Item("o2", ["type": "ask", "question": "which room"])])[0].result
        let a = await kernel.send([Item("o1", ["type": "reflect"])])[0].result
        let b = await kernel.send([Item("o2", ["type": "reflect"])])[0].result
        #expect(a["asked"].asInt == 3, "\(a)")
        #expect(b["asked"].asInt == 1, "\(b)")
    }
}
