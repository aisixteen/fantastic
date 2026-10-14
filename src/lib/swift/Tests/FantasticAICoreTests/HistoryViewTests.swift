// The MODEL VIEW trimmers, tested as the pure functions they are. Mirrors the
// canonical python suite `ai_core/tests/test_context.py:509-634` and rust's
// `history_view.rs` unit tests.
//
// Repeated whole-state replies (reflect above all) are a large share of a
// conversation. That bulk is what fills the window; a full window is what forces
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

    /// A reflect is MUTABLE state: its body stays whole after
    /// the model answers it; only a newer reflect of the same agent supersedes it
    /// (`dedupeStaleReads`). Only `docs` replies are cut.
    @Test func aReflectReplyIsNeverCutFromTheView() {
        let tree = "<id>notes</id>\n<sentence>" + String(repeating: "x", count: 400) + "</sentence>"
        let body = [
            turn("user", "[reply to notes:aaa · notes reflect]\n" + tree),
            turn("assistant", #"<call name="send"><target_id>notes</target_id><type>ps</type></call>"#),
            turn("user", "next"),
        ]
        #expect(dropReadDocs(dedupeStaleReads(body)) == body)
    }

    /// A batch arrives as ONE turn holding several `[reply to …]` blocks. The
    /// trimmer works per BLOCK: a superseded whole-state block dedupes alone, its
    /// sibling replies stay. Treating the turn as one reply either deletes siblings
    /// or trims nothing (a count guard skips every multi-reply turn).
    @Test func aCombinedTurnTrimsPerBlockNeverWhole() {
        let combined =
            "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch ab12]\n"
            + String(repeating: "TREE", count: 100)
            + "\n\n[reply to tot:b1 · totals read · 2 of 2 in batch ab12]\nreading_1 = 41"
        let body = [
            turn("user", combined),
            turn("assistant", "noted"),
            turn("user", "[reply to ks:a2 · kernel_state reflect]\nNEW TREE"),
        ]
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

    /// A verb group of `\S+` does not exclude `]`, so on a reply whose BODY carries
    /// a bracket the run would keep `reflect]` and the regex would close on the
    /// body's bracket — the verb would match no vocabulary, and BOTH trimmers would
    /// silently do nothing on every reflect, since reflect bodies are JSON.
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
    /// absorbs the batch suffix.
    @Test func theTagMatcherIsPythons() {
        #expect(parseReplyTag("[reply to a:1 · notes reflect]\nbody")?.agent == "notes")
        #expect(parseReplyTag("[reply to a:1 · notes reflect]\nbody")?.verb == "reflect")
        let stamped = "[reply to a:1 · notes reflect · 2 of 3 in batch ab12]\nbody"
        #expect(parseReplyTag(stamped)?.verb == "reflect")
        #expect(parseReplyTag("no tag here") == nil)
        #expect(parseReplyTag("[reply to a:1 notes reflect]") == nil)
    }
}
