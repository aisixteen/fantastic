// The one refusal shape — the CLOSED code set and the facts each carries.

import FantasticJSON
import Testing

@testable import FantasticKernel

@Suite("Refusal: one shape, one machine field")
struct RefusalTests {
    @Test func everyRefusalCarriesBothFields() {
        let v = Refusal.make(Refusal.missingArg, "create_agent: parent_id required")
        #expect(v["error"].asString == "create_agent: parent_id required")
        #expect(v["reason"].asString == "missing_arg")
        // Nothing else unless asked for — `hint`/`see` are opt-in.
        #expect(v.asObject?.count == 2)
    }

    @Test func missingAndBadNameTheArgument() {
        let m = Refusal.missing("create_agent", "parent_id")
        #expect(m["reason"].asString == "missing_arg")
        #expect(m["arg"].asString == "parent_id")

        let b = Refusal.bad("send", "timeout", "a positive number")
        #expect(b["reason"].asString == "bad_arg")
        #expect(b["arg"].asString == "timeout")
        #expect(b["error"].asString == "send: timeout must be a positive number")
    }

    @Test func notHerePointsAtTheRoot() {
        // The operand/target confusion is the whole reason this code exists, so
        // the FIX rides in the refusal.
        let v = Refusal.notHere("create_agent", "some_child")
        #expect(v["reason"].asString == "not_here")
        // PYTHON'S WORDS. One code must not mean two sentences across runtimes.
        #expect(v["error"].asString == "create_agent is a kernel verb — some_child does not answer it")
        let hint = v["hint"].asString ?? ""
        #expect(hint.contains("kernel"))
        #expect(hint.contains("`parent_id` / `id`"))
        // The machine field: which verb was misaddressed, without parsing prose.
        #expect(v["verb"].asString == "create_agent")
    }

    @Test func unknownAgentCarriesTheLiveIndex() {
        let v = Refusal.unknown("typo", live: ["kernel_state", "cli", "file_bridge_ab12"])
        #expect(v["reason"].asString == "unknown_agent")
        #expect(v["agent_count"].asInt == 3)
        // Sorted, so a caller reading them gets a stable list.
        #expect(v["agents"][0].asString == "cli")
        #expect(v["agents"][1].asString == "file_bridge_ab12")
        #expect(v["agents"][2].asString == "kernel_state")
        #expect(v["hint"] == .null, "a real id needs no placeholder hint")
    }

    @Test func aPlaceholderIdIsNamedAsOne() {
        // Measured 2026-08-04: a model copied `<id>` out of an example and got
        // the same bare refusal 34 times without ever correcting.
        let v = Refusal.unknown("<id>", live: ["kernel_state"])
        let hint = v["hint"].asString ?? ""
        #expect(hint.contains("PLACEHOLDER"))
        #expect(hint.contains("agents"))
    }

    @Test func theIdIndexIsBounded() {
        let ids = (0..<200).map { "agent_\($0)" }
        let v = Refusal.unknown("nope", live: ids)
        // The count stays honest even though the list is clipped — one typo
        // must not become a context dump.
        #expect(v["agent_count"].asInt == 200)
        #expect(v["agents"].asArray?.count == Refusal.unknownAgentIdsMax)
    }

    @Test func extraFactsRideAtTheTopLevel() {
        let v = Refusal.make(
            Refusal.unauthorized, "a may not call \"b\" on c",
            extra: [("side", .string("egress"))])
        #expect(v["side"].asString == "egress")
        #expect(v["reason"].asString == "unauthorized")
    }

    // MARK: - python repr (C19)

    /// Python writes `f"no agent {agent_id!r}"` — SINGLE quotes, and the repr
    /// rules for a quote or a control character inside the id.
    @Test func unknownAgentErrorIsPythonRepr() {
        #expect(
            Refusal.unknown("ghost", live: ["kernel_state"])["error"].asString == "no agent 'ghost'")
        // A `'` inside flips the quote, exactly as CPython's `unicode_repr` does.
        #expect(
            Refusal.unknown("it's", live: ["kernel_state"])["error"].asString == "no agent \"it's\"")
        // TAB/CR are escaped — the kernel's own copy of repr escaped neither.
        #expect(
            Refusal.unknown("a\tb\rc", live: ["kernel_state"])["error"].asString
                == "no agent 'a\\tb\\rc'")
    }

    /// Python assigns `reply["hint"]` AFTER building the refusal, so the hint
    /// lands BEHIND `agent_count` / `agents`, and quotes the id with repr.
    @Test func placeholderHintFollowsAgents() {
        let v = Refusal.unknown("<id>", live: ["kernel_state", "cli"])
        #expect(v.asObject?.keys.elements == ["error", "reason", "agent_count", "agents", "hint"])
        #expect(
            v["hint"].asString
                == "'<id>' is a PLACEHOLDER from an example, not an id — "
                + "substitute one of `agents`.")
        #expect(v["error"].asString == "no agent '<id>'")
    }

    /// `guard_agent_id` spells the offending id with `{agent_id!r}`.
    /// `kernel` names the operator; `|` marks a FOREIGN `guid|agent` (audit
    /// 2026-09-28: `create_agent id=kernel` made an agent the operator to every
    /// memory store).
    @Test func theReservedIdAndAForeignIdAreRefused() {
        #expect(
            Refusal.guardAgentId("create_agent", "kernel")?["error"].asString
                == "create_agent: id must be a name other than the reserved 'kernel'")
        #expect(
            Refusal.guardAgentId("create_agent", "a|b")?["error"].asString
                == "create_agent: id must be a local name without `|` (it marks a foreign `guid|agent`); got 'a|b'")
        #expect(Refusal.guardAgentId("create_agent", "kernel_notes") == nil)
    }

    @Test func guardAgentIdQuotesWithPythonRepr() {
        // Python tests the separator FIRST, so `../x` is refused as a separator.
        #expect(
            Refusal.guardAgentId("create_agent", "../x")?["error"].asString
                == "create_agent: id must be a single path segment (no separators); got '../x'")
        #expect(
            Refusal.guardAgentId("create_agent", "a..b")?["error"].asString
                == "create_agent: id must be a name that is not a path traversal; got 'a..b'")
        #expect(
            Refusal.guardAgentId("create_agent", ".hidden")?["error"].asString
                == "create_agent: id must be a name that is not hidden; got '.hidden'")
        #expect(
            Refusal.guardAgentId("create_agent", "123")?["error"].asString
                == "create_agent: id must be a name, not a number (message ids are ints); "
                + "got '123'")
        // `:` is a legal id character — python's guard never mentions it.
        #expect(Refusal.guardAgentId("create_agent", "ai:main") == nil)
    }
}
