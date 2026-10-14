// The ONE scalar rule for text a caller typed.
//
// Python keeps it in `kernel/modes/parse.py` as `coerce_scalar`, and TWO paths
// share it: the CLI's `k=v` and the model's `<k>v</k>`. It lives in the kernel
// module here for the same reason rust put it in `fantastic-kernel` — the AI
// core and the CLI executable both depend on this and neither depends on the
// other, so this is the only place one rule can serve both.
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
/// NOT Foundation's `.whitespacesAndNewlines`, and NOT a `Character`-at-a-time
/// walk either. Python's `str.strip()` removes SCALARS where `str.isspace()`
/// is true (`pyIsSpace`, which also covers U+001C..U+001F). A `Character` scan
/// strips too much: `" \u{301}x"` (a bare combining acute accent after a space)
/// is ONE swift grapheme cluster whose first scalar is the space, so a
/// `Character`-granularity strip drops the combining mark along with it, where
/// python — which strips scalars, not clusters — keeps it (`" ́x".strip() ==
/// "́x"`, the mark still attached to nothing).
public func pyStrip(_ s: String) -> String {
    let scalars = Array(s.unicodeScalars)
    var start = 0
    var end = scalars.count
    while start < end, pyIsSpace(scalars[start]) { start += 1 }
    while end > start, pyIsSpace(scalars[end - 1]) { end -= 1 }
    return String(String.UnicodeScalarView(scalars[start..<end]))
}

/// Type one scalar the way python's `coerce_scalar` does: `try: int(v) / except
/// ValueError: try: float(v) / except ValueError: return v`. Built ON `pyInt`/
/// `pyFloat` (`FantasticJSON/PyFormat.swift`) rather than a second hand-rolled
/// parse, so every CPython grammar quirk they already mirror carries over here
/// for free:
///
/// - the bool test is case-insensitive (`v.lower() in ("true","false")`);
/// - `int()` and `float()` accept `_` between digits (`1_000`) and a leading
///   `+`, and reject a leading, trailing or doubled underscore;
/// - `int()` and `float()` map EVERY Unicode decimal digit (any script) to its
///   ASCII value before parsing — CPython's
///   `_PyUnicode_TransformDecimalAndSpaceToASCII` — so scripts may even mix
///   within one literal (`int('1١')` is 11, both digits meaning 1);
/// - `float()` accepts `inf`, `infinity` and `nan` in any case, with a sign,
///   and REJECTS every spelling outside its plain decimal-literal grammar —
///   no hex float (`0x10`, `0x1p3`) and no NaN payload (`nan(1)`), both of
///   which a bare `Double(text)` would have taken.
///
/// ONE value python holds that this cannot: an integer wider than `Int64`
/// (python's ints are unbounded, and going through `Double` would silently
/// lose digits) — `pyInt` reports that as `OverflowError`, caught here and
/// kept as the string it was written as, never promoted to a double. Non-finite
/// doubles ARE representable here (`JSON.double`), unlike in rust where
/// `serde_json` has no such number.
public func coerceScalar(_ v: String) -> JSON {
    let lowered = v.lowercased()
    if lowered == "true" { return .bool(true) }
    if lowered == "false" { return .bool(false) }
    switch coerceScalarIntPhase(v) {
    case .value(let i): return .integer(i)
    case .tooWide: return .string(v)
    case .notAnInt: break
    }
    if let d = try? pyFloat(.string(v)) { return .double(d) }
    return .string(v)
}

/// True when python's `coerce_scalar` would type `v` as an int — an ordinary
/// `Int64`-shaped one AND one too wide for that carrier (`coerceScalar` keeps
/// the latter as `.string`). A caller that must refuse a
/// numeric-shaped value the way python's `isinstance(x, str)` gate does —
/// e.g. a `<target_id>` naming a send's recipient, which python types as an
/// `int` and therefore never dispatches as a string id — checks this instead
/// of trusting `coerceScalar(v)`'s `.string` case alone.
public func coerceScalarIsIntegerShaped(_ v: String) -> Bool {
    if case .notAnInt = coerceScalarIntPhase(v) { return false }
    return true
}

private enum IntPhase {
    case value(Int64)
    case tooWide, notAnInt
}

private func coerceScalarIntPhase(_ v: String) -> IntPhase {
    do {
        return .value(Int64(try pyInt(.string(v))))
    } catch {
        return error.type == "OverflowError" ? .tooWide : .notAnInt
    }
}
