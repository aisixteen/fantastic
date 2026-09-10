// The ONE scalar rule for text a caller typed.
//
// Python keeps it in `kernel/modes/parse.py` as `coerce_scalar`, and TWO paths
// share it: the CLI's `k=v` and the model's `<k>v</k>`. It lives in the kernel
// module here for the same reason rust put it in `fantastic-kernel` — the AI
// core and the CLI executable both depend on this and neither depends on the
// other, so this is the only place one rule can serve both. Until 2026-09-10 the
// swift copy lived inside the CLI (`Fantastic/REPL.swift`), where the model path
// could not reach it at all.
//
// `true`/`false` → bool, an integer → integer, a decimal → double, anything else
// → the string as written. No JSON, no quoting: a value that has to stay text is
// text, and a verb that needs a type gets the typed value or refuses. The CLI
// layers a JSON object/list branch on top for the OPERATOR; the model↔harness
// path carries no JSON at all.

import FantasticJSON
import Foundation

/// `s` with python's whitespace stripped from both ends.
///
/// NOT Foundation's `.whitespacesAndNewlines`. Python's `str.strip()` removes
/// every character where `str.isspace()` is true, and that set includes the four
/// ASCII separators U+001C..U+001F, which Unicode's whitespace class does not.
/// One character of difference matters only because a value that keeps a stray
/// separator stops being a scalar the far side recognises.
public func pyStrip(_ s: String) -> String {
    let strippable: (Character) -> Bool = { c in
        c.isWhitespace || ("\u{1c}"..."\u{1f}").contains(c)
    }
    var out = Substring(s)
    while let f = out.first, strippable(f) { out = out.dropFirst() }
    while let l = out.last, strippable(l) { out = out.dropLast() }
    return String(out)
}

/// Type one scalar the way python's `coerce_scalar` does.
///
/// CPython parity notes, each a real difference from a naive `Int64(v)`:
///
/// - the bool test is case-insensitive (`v.lower() in ("true","false")`);
/// - `int()` and `float()` accept `_` between digits (`1_000`) and a leading
///   `+`, and reject a leading, trailing or doubled underscore;
/// - `float()` accepts `inf`, `infinity` and `nan` in any case, with a sign.
///
/// ONE value python holds that this cannot: an integer wider than `Int64`.
/// Python's ints are unbounded, and going through `Double` would silently lose
/// digits — so a too-wide integer stays the string it was written as. Non-finite
/// doubles ARE representable here (`JSON.double`), unlike in rust where
/// `serde_json` has no such number.
public func coerceScalar(_ v: String) -> JSON {
    let lowered = v.lowercased()
    if lowered == "true" { return .bool(true) }
    if lowered == "false" { return .bool(false) }
    guard let text = withoutUnderscores(v) else { return .string(v) }
    if let i = Int64(text) { return .integer(i) }
    // AN INTEGER TOO WIDE FOR Int64 IS NOT A DOUBLE. `Double(text)` would take
    // it and lose digits; the text stands instead.
    if isIntegerShaped(text) { return .string(v) }
    if let d = pyDouble(text) { return .double(d) }
    return .string(v)
}

/// True when the text is a plain signed run of digits — an integer python holds
/// exactly and `Double` would round.
private func isIntegerShaped(_ text: String) -> Bool {
    var digits = Substring(text)
    if let f = digits.first, f == "+" || f == "-" { digits = digits.dropFirst() }
    return !digits.isEmpty && digits.allSatisfy(\.isASCII) && digits.allSatisfy(\.isNumber)
}

/// `Double(text)` plus the spellings python accepts and swift does not.
private func pyDouble(_ text: String) -> Double? {
    if let d = Double(text) { return d }
    var body = Substring(text)
    var sign = 1.0
    if let f = body.first, f == "+" || f == "-" {
        if f == "-" { sign = -1 }
        body = body.dropFirst()
    }
    switch body.lowercased() {
    case "inf", "infinity": return sign * Double.infinity
    case "nan": return Double.nan
    default: return nil
    }
}

/// `v` with python's legal digit-separating underscores removed, or `nil` when
/// an underscore sits where python would refuse it: at either end, beside
/// another underscore, or next to anything but a digit on both sides.
private func withoutUnderscores(_ v: String) -> String? {
    guard v.contains("_") else { return v }
    let chars = Array(v)
    var out = ""
    out.reserveCapacity(chars.count)
    for (i, c) in chars.enumerated() {
        if c != "_" {
            out.append(c)
            continue
        }
        guard i > 0, i + 1 < chars.count else { return nil }
        guard chars[i - 1].isASCII, chars[i - 1].isNumber,
              chars[i + 1].isASCII, chars[i + 1].isNumber
        else { return nil }
    }
    return out
}
