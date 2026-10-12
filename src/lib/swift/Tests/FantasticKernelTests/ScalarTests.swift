// `pyStrip` / `coerceScalar` — the ONE scalar rule, pinned against CPython.

import FantasticJSON
import Testing

@testable import FantasticKernel

@Suite("Scalar")
struct ScalarTests {
    // MARK: - C71: pyStrip strips unicode scalars, not Characters

    @Test func pyStripRemovesScalarsNotCharacters() {
        // U+0020 SPACE followed by U+0301 COMBINING ACUTE ACCENT then "x" is ONE
        // swift grapheme cluster (the accent combines onto the space). Python
        // strips only the leading SCALAR (the space), keeping the combining mark
        // attached to nothing — `" ́x".strip() == "́x"`.
        #expect(pyStrip(" \u{301}x") == "\u{301}x")
        // The four ASCII separators U+001C..U+001F count as whitespace to
        // python's `str.isspace()`, interspersed with ordinary digits.
        #expect(pyStrip("\u{1f}12\u{1c}") == "12")
        #expect(pyStrip("  hello  ") == "hello")
        #expect(pyStrip("") == "")
        #expect(pyStrip("\u{1c}\u{1d}\u{1e}\u{1f}") == "")
    }

    // MARK: - C72: coerce_scalar accepts Unicode decimal digits

    @Test func unicodeDigitsFollowCPython() {
        // Mixed scripts within ONE literal: ASCII '1' + Arabic-Indic '١' (value
        // 1) reads as the two-digit integer 11, exactly as CPython's
        // `int('1١')` does.
        #expect(coerceScalar("1\u{0661}") == .integer(11))
        // A single full-width digit string.
        #expect(coerceScalar("\u{FF11}\u{FF12}") == .integer(12))
        // A float built from Arabic-Indic digits.
        if case .double(let d) = coerceScalar("\u{0661}.\u{0665}") {
            #expect(d == 1.5)
        } else {
            Issue.record("expected a double")
        }
        #expect(coerceScalarIsIntegerShaped("1\u{0661}"))
    }

    // MARK: - C73: coerceScalar rejects float spellings python's float() rejects

    @Test func hexAndNanPayloadStayText() {
        // CPython's `float()` grammar has no hex form and no NaN payload —
        // both stay text, where a bare `Double(text)` would have parsed them.
        #expect(coerceScalar("0x10") == .string("0x10"))
        #expect(coerceScalar("0x1p3") == .string("0x1p3"))
        #expect(coerceScalar("nan(1)") == .string("nan(1)"))
        // Python DOES accept a bare trailing/leading dot.
        #expect(coerceScalar("5.") == .double(5.0))
        #expect(coerceScalar(".5") == .double(0.5))
    }

    // MARK: - C74 (D13): a wide integer is int-shaped even though it can't fit
    // this carrier's Int64 — `coerceScalar` keeps the text, but a caller that
    // needs python's `isinstance(x, str)` gate (a send's `target_id`) must
    // still see it as non-string.

    @Test func wideIntegerStaysAnInteger() {
        let huge = "123456789012345678901234567890"
        // The JSON carrier has no bignum case (D13 residual): the too-wide
        // value stays the string it was written as, never silently rounded
        // through a `Double`.
        #expect(coerceScalar(huge) == .string(huge))
        // But it is still RECOGNIZED as integer-shaped, unlike an ordinary
        // string — this is what lets a `<target_id>` of this shape refuse
        // dispatch instead of naming a literal numeral agent id (Grammar.swift).
        #expect(coerceScalarIsIntegerShaped(huge))
        #expect(!coerceScalarIsIntegerShaped("not_a_number"))
        #expect(!coerceScalarIsIntegerShaped("12.5"))
        // An ordinary in-range integer is still typed, not stringified.
        #expect(coerceScalar("42") == .integer(42))
        #expect(coerceScalarIsIntegerShaped("42"))
    }
}
