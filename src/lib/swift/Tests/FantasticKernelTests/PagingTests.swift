// Paging — the default must be unchanged, and the cursor must be opaque.

import FantasticJSON
import Testing

@testable import FantasticKernel

@Suite("Paging: bounded replies, opaque cursors")
struct PagingTests {
    private func ids(_ v: [String]) -> [String] { v }
    private func me(_ s: String) -> String { s }

    @Test func noLimitReturnsEverythingAndNoCursor() {
        // The whole point: a caller that never learned about paging is unaffected.
        let (p, next) = page(ids(["a", "b", "c"]), .object([:]), ident: me)
        #expect(p == ["a", "b", "c"])
        #expect(next == nil)
    }

    @Test func aLimitBoundsThePageAndNamesTheCursor() {
        let (p, next) = page(
            ids(["a", "b", "c", "d"]), .object(["limit": .integer(2)]), ident: me)
        #expect(p == ["a", "b"])
        // The cursor is the last item's IDENTITY, not an offset — a caller
        // cannot compute the next one by arithmetic.
        #expect(next == "b")
    }

    @Test func theLastPageReportsNoCursor() {
        let (p, next) = page(ids(["a", "b"]), .object(["limit": .integer(2)]), ident: me)
        #expect(p == ["a", "b"])
        #expect(next == nil, "a full-but-final page must not promise more")
    }

    @Test func afterResumesStrictlyPastTheNamedItem() {
        let (p, next) = page(
            ids(["a", "b", "c", "d"]),
            .object(["after": .string("b"), "limit": .integer(2)]), ident: me)
        #expect(p == ["c", "d"])
        #expect(next == nil)
    }

    @Test func walkingTheWholeListByCursorVisitsEachItemOnce() {
        let all = ids(["a", "b", "c", "d", "e"])
        var seen: [String] = []
        var cursor: String? = nil
        while true {
            var payload: JSON = .object(["limit": .integer(2)])
            if let c = cursor {
                payload = .object(["limit": .integer(2), "after": .string(c)])
            }
            let (p, next) = page(all, payload, ident: me)
            seen.append(contentsOf: p)
            guard let n = next else { break }
            cursor = n
        }
        #expect(seen == all)
    }

    @Test func aCursorNamingSomethingGoneReturnsEmptyNotTheTop() {
        // Silently restarting from the top would look like a successful walk and
        // hand the caller the same items forever.
        let (p, next) = page(
            ids(["a", "b"]), .object(["after": .string("deleted")]), ident: me)
        #expect(p.isEmpty)
        #expect(next == nil)
    }

    @Test func aNonsenseLimitMeansEverything() {
        for bad: JSON in [.integer(0), .integer(-3), .string("2"), .bool(true), .null] {
            let (p, next) = page(ids(["a", "b", "c"]), .object(["limit": bad]), ident: me)
            #expect(p.count == 3, "limit \(bad) should not bound the page")
            #expect(next == nil)
        }
    }

    @Test func anEmptyAfterIsIgnored() {
        let (p, _) = page(ids(["a", "b"]), .object(["after": .string("")]), ident: me)
        #expect(p == ["a", "b"])
    }

    // ─── after is python `str(after)` for ANY value ────

    @Test func anIntAfterMatchesItsDecimalString() {
        let (p, _) = page(ids(["1", "2", "3"]), .object(["after": .integer(2)]), ident: me)
        #expect(p == ["3"], "\(p)")
    }

    @Test func aFloatAfterStringifiesAsPythonRepr() {
        // `str(2.0)` is `"2.0"`, never truncated to `"2"` — it matches NOTHING
        // here even though an item id `"2"` exists.
        let (p, next) = page(ids(["1", "2", "3"]), .object(["after": .double(2.0)]), ident: me)
        #expect(p.isEmpty, "\(p)")
        #expect(next == nil)
    }

    @Test func aNonWholeFloatAfterAlsoStringifies() {
        let (p, next) = page(ids(["1", "2", "3"]), .object(["after": .double(1.5)]), ident: me)
        #expect(p.isEmpty, "\(p)")
        #expect(next == nil)
    }

    @Test func anArrayOrObjectAfterStringifiesAsPythonRepr() {
        for bad: JSON in [.array([.string("c1")]), .object(["k": .string("v")])] {
            let (p, next) = page(ids(["1", "2", "3"]), .object(["after": bad]), ident: me)
            #expect(p.isEmpty, "\(bad): \(p)")
            #expect(next == nil)
        }
    }

    @Test func aBoolAfterIsNoCursorEitherWay() {
        for b: JSON in [.bool(true), .bool(false)] {
            let (p, next) = page(ids(["1", "2", "3"]), .object(["after": b]), ident: me)
            #expect(p == ["1", "2", "3"], "\(b): \(p)")
            #expect(next == nil)
        }
    }

    @Test func aNullAfterIsNoCursor() {
        let (p, _) = page(ids(["1", "2", "3"]), .object(["after": .null]), ident: me)
        #expect(p == ["1", "2", "3"])
    }
}
