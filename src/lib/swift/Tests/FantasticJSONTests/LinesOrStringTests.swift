// `code` / `content` as a LIST OF LINES.
//
// A program written as one JSON string is the most fragile thing a model emits:
// every newline is an escape, every inner quote another, and one missed `\"`
// swallows the rest of the object. These pin the shape that removes the failure
// rather than softening it.

import FantasticJSON
import Testing

@Suite("lines or string")
struct LinesOrStringTests {
    @Test("a list of lines joins with newlines")
    func listJoins() {
        #expect(linesOrString(.array([.string("a"), .string("b")])) == "a\nb")
    }

    @Test("a plain string is unchanged")
    func stringPassthrough() {
        #expect(linesOrString(.string("x = 1")) == "x = 1")
    }

    @Test("a bare number is REFUSED, not stringified")
    func numberRefused() {
        // The list is the ONLY new shape. `42` is a mistake, not a one-line
        // program, and stringifying it would spawn an interpreter on nonsense.
        #expect(linesOrString(.integer(42)) == nil)
        #expect(linesOrString(nil) == nil)
    }

    @Test("a null element becomes an empty line, not the text 'null'")
    func nullElement() {
        #expect(linesOrString(.array([.string("a"), .null, .string("b")])) == "a\n\nb")
    }
}
