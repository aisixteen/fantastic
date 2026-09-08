// The MODEL VIEW trimmers, tested as the pure functions they are. Mirrors the
// canonical python suite `ai_core/tests/test_context.py:509-634` and rust's
// `history_view.rs` unit tests.
//
// MEASURED (2026-08-20, `live_plan`): reflect replies were 42% of the whole
// conversation — 73k of 174k chars — and 9 of 16 were a repeat of a target already
// reflected. That bulk is what fills the window; a full window is what forces
// compaction; compaction is what put the goal at risk. So the reflect body is kept
// for exactly one turn — the turn the model reads it — and after the model has
// answered, only the distillation IT wrote survives.

import FantasticJSON
import Testing

@testable import FantasticAICore

private func turn(_ role: String, _ content: String) -> JSON {
    .object(["role": .string(role), "content": .string(content)])
}

private func texts(_ turns: [JSON]) -> [String] {
    turns.map { $0["content"].asString ?? "" }
}

@Suite("AI history view: what the prompt carries, never the store")
struct HistoryViewTests {

    @Test func staleWholeStateReadsDedupeButParameterisedReadsNeverDo() {
        let body = [
            turn("user", "[reply to ks:a1 · kernel_state reflect]\nOLD TREE"),
            turn("assistant", "noted"),
            turn("user", "[reply to fb:b1 · readings read]\nreading_1 = 41"),
            turn("user", "[reply to fb:b2 · readings read]\nreading_2 = 42"),
            turn("user", "[reply to ks:a2 · kernel_state reflect]\nNEW TREE"),
        ]
        let all = texts(dedupeStaleReads(body)).joined(separator: "\n")
        // whole-state: the newest reflect supersedes the older one
        #expect(!all.contains("OLD TREE"))
        #expect(all.contains("NEW TREE"))
        // parameterised: two `read`s are DIFFERENT FILES — dropping either loses
        // data, and the reply tag carries no args to tell them apart.
        #expect(all.contains("reading_1 = 41"))
        #expect(all.contains("reading_2 = 42"))
    }

    @Test func answeredReflectsAreStubbedButTheInFlightOneSurvives() {
        let body = [
            turn(
                "user",
                "[reply to notes:aaa · notes reflect]\n"
                    + String(repeating: "TREE", count: 200)),
            turn("assistant", "SEEN notes — key/value store; verbs read/set/keys."),
            turn(
                "user",
                "[reply to assets:bbb · assets reflect]\n"
                    + String(repeating: "TREE", count: 200)),
        ]
        let out = dropAnsweredReflects(body)
        // answered → stubbed, and the stub still NAMES the agent, so "I already
        // looked at this" stays true and the model does not re-reflect out of doubt
        #expect(!texts(out)[0].contains("TREE"))
        #expect(texts(out)[0].contains("notes"))
        // the model's own distillation is untouched — it is the record now
        #expect(out[1] == body[1])
        // the IN-FLIGHT reflect is verbatim: the model is reading it this very turn
        #expect(out[2] == body[2])
    }

    /// FAIL CLOSED. Measured 2026-08-20 (`live_plan`): the model answered several
    /// reflects with a bare tool_call and no prose. The body was dropped anyway, so
    /// the facts were simply gone — "The index was dropped from my view. Let me send
    /// it again and, THIS TIME, actually record the facts." Re-fetching what it lost
    /// is the exact loop this trim exists to prevent, so a reflect the model did not
    /// distil must survive: non-compliance costs context, never comprehension.
    @Test func aReflectTheModelDidNotDistilIsKept() {
        let body = [
            turn(
                "user",
                "[reply to notes:aaa · notes reflect]\n"
                    + String(repeating: "TREE", count: 100)),
            turn("assistant", #"<tool_call>{"name":"send",...}</tool_call>"#),  // no SEEN
            turn(
                "user",
                "[reply to assets:bbb · assets reflect]\n"
                    + String(repeating: "FACTS", count: 100)),
            turn("assistant", "SEEN assets — public web assets; verbs read/list."),
            turn("user", "next"),
        ]
        let out = texts(dropAnsweredReflects(body))
        #expect(out[0].contains("TREE"), "undistilled reflect must be KEPT")
        #expect(!out[2].contains("FACTS"), "distilled reflect is dropped")
        #expect(out[2].contains("assets"), "the stub still names the agent")
    }

    /// A batch arrives as ONE turn holding several `[reply to …]` blocks. Both
    /// trimmers work per BLOCK: a distilled reflect block drops alone, its sibling
    /// replies stay; a superseded whole-state block dedupes alone. Treating the turn
    /// as one reply either deleted siblings (`dropAnsweredReflects` replaced the
    /// WHOLE content) or trimmed nothing (a count guard skipped every multi-reply
    /// turn — tried 2026-08-25, superseded same day).
    @Test func aCombinedTurnTrimsPerBlockNeverWhole() {
        let combined =
            "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch ab12]\n"
            + String(repeating: "TREE", count: 100)
            + "\n\n[reply to tot:b1 · totals read · 2 of 2 in batch ab12]\nreading_1 = 41"
        let body = [
            turn("user", combined),
            turn("assistant", "SEEN kernel_state — the root; verbs create/list."),
            turn("user", "[reply to ks:a2 · kernel_state reflect]\nNEW TREE"),
        ]
        let out = texts(dropAnsweredReflects(body))
        #expect(
            !out[0].contains(String(repeating: "TREE", count: 5)),
            "distilled reflect block drops")
        #expect(out[0].contains("reading_1 = 41"), "sibling block must SURVIVE")

        let out2 = texts(dedupeStaleReads(body))
        #expect(
            !out2[0].contains(String(repeating: "TREE", count: 5)),
            "older whole-state block is superseded")
        #expect(out2[0].contains("reading_1 = 41"), "parameterised sibling must survive")
        #expect(out2.contains { $0.contains("NEW TREE") })
    }

    @Test func aTurnWhoseEveryBlockIsSupersededDropsEntirely() {
        let body = [
            turn(
                "user",
                "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch cd34]\nOLD"
                    + "\n\n[reply to k:a2 · kernel ps · 2 of 2 in batch cd34]\nOLD ROWS"),
            turn("assistant", "noted"),
            turn("user", "[reply to ks:a3 · kernel_state reflect]\nNEW"),
            turn("user", "[reply to k:a4 · kernel ps]\nNEW ROWS"),
        ]
        let out = dedupeStaleReads(body)
        #expect(!texts(out).contains { $0.contains("OLD") })
        #expect(out.count == 3, "the emptied turn leaves no husk behind")
    }

    /// MEASURED 2026-09-04, in all three runtimes at once. The verb group was `\S+`,
    /// which does not exclude `]`, so on a reply whose BODY carries a bracket the run
    /// kept `reflect]` and the regex closed on the body's bracket. The verb then read
    /// `reflect]`, matched no vocabulary, and BOTH trimmers silently did nothing — on
    /// every reflect, since reflect bodies are JSON and always contain one.
    ///
    /// It survived because every fixture here, in python and in rust, used a
    /// bracket-free body. The three runtimes agreed with each other and with nothing.
    @Test func theVerbStopsAtTheTagsOwnBracketNotTheBodys() {
        let real = #"[reply to notes:aa · notes reflect]\nverbs=["read"]  root=."#
        #expect(parseReplyTag(real)?.verb == "reflect")
        let stamped = #"[reply to notes:aa · notes reflect · 2 of 3 in batch ab12]\nv=["x"]"#
        #expect(parseReplyTag(stamped)?.verb == "reflect")
    }

    @Test func aStaleReflectWhoseBodyHasABracketStillDedupes() {
        let body = #"verbs=["read", "write"]  root=."#
        let hist: [JSON] = [
            ["role": "user", "content": .string("[reply to n:aa · notes reflect]\n" + body)],
            ["role": "assistant", "content": "SEEN notes"],
            ["role": "user", "content": .string("[reply to n:bb · notes reflect]\n" + body)],
        ]
        let out = dedupeStaleReads(hist)
        let kept = out.filter { ($0["content"].asString ?? "").contains(body) }.count
        #expect(kept == 1, "stale whole-state reflect not dropped")
    }

    /// The tag matcher is python's regex, hand-scanned — including the `[^\]]*` that
    /// absorbs the batch suffix, and the fail-closed `SEEN` word boundary.
    @Test func theTagMatcherAndTheSeenBoundaryArePythons() {
        #expect(parseReplyTag("[reply to a:1 · notes reflect]\nbody")?.agent == "notes")
        #expect(parseReplyTag("[reply to a:1 · notes reflect]\nbody")?.verb == "reflect")
        let stamped = "[reply to a:1 · notes reflect · 2 of 3 in batch ab12]\nbody"
        #expect(parseReplyTag(stamped)?.verb == "reflect")
        #expect(parseReplyTag("no tag here") == nil)
        #expect(parseReplyTag("[reply to a:1 notes reflect]") == nil)
        // `\bSEEN\b`, any case, anywhere — and NOT inside a longer word.
        #expect(distilled("seen notes — a store"))
        #expect(distilled("I have SEEN it."))
        #expect(!distilled("unforeseen consequences"))
        #expect(!distilled("SEENING"))
        #expect(!distilled("a bare tool call and no prose"))
    }
}
