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
    let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    var rec: JSON = [
        "type": "create_agent", "parent_id": "core",
        "handler_module": .string(ORACLE_MODULE), "id": "oracle",
    ]
    if let lines { rec["lines"] = lines }
    let out = await kernel.send([Item("core", rec)])[0].result
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
        #expect(
            r["verbs"]["ask"].asString
                == "args: question:str (req), limit:int? (default 6). Keyword search over my ledger — the lines matching the most words of the question come back as {answers:[str], matched:int}. Ask precisely; a vague question returns noise."
        )
        #expect(r["verbs"].asObject?.count == 1)
        #expect(r["asked"].asInt == 0)
        // The bundle's own fields, in python's order; the substrate appends its frame after them.
        #expect(r.asObject?.keys.starts(with: ["id", "sentence", "verbs", "asked"]) == true, "\(r)")
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
}
