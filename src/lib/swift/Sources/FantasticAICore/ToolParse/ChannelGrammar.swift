// The `<|tool_call>` body's VALUE grammar — quoted strings, bare scalars,
// arrays and nested objects.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`CHANNEL_STR`,
// `_ESCAPES`, `_parse_quoted`, `_parse_channel_value`, `_parse_channel_object`).
// The COMMENTS are part of that port: each records a MEASURED failure with its
// date, and it is the only thing that stops the behaviour being "simplified"
// away later.
//
// This body is NOT JSON. Keys are bare, strings wear a vocabulary token of their
// own (`<|"|>`), and numbers, booleans, lists and nested objects are written
// bare. `JSON.parse` over such a body succeeds ONLY for the empty `{}` — every
// call carrying an actual argument would parse as malformed and be dropped,
// which is the worst possible failure here.
//
// Python signals "malformed" with a `_BAD` sentinel plus a `-1` index, because
// `None` is a legal VALUE and cannot double as the failure signal. Swift says
// the same thing with `nil` on the whole `(value, next)` pair, so a parsed
// `.null` and a parse failure can never be confused.

import FantasticJSON
import Foundation
import OrderedCollections

/// The quoting TOKEN this grammar uses instead of `"`. A `|` inside the brackets
/// marks a token from the tokenizer's VOCABULARY, so the delimiter cannot be
/// forged by the content it wraps — which is also why the content needs no
/// escaping and may contain `,` `}` `]` freely.
let CHANNEL_STR = #"<|"|>"#
/// `CHANNEL_STR` as `[Character]`, since the readers index character arrays.
let CHANNEL_STR_CHARS = Array(CHANNEL_STR)

/// The escapes a quoted channel string may carry.
///
/// Shared by both quote styles so `'a\nb'` and `"a\nb"` decode identically — a
/// model should not get a different string back for picking a different quote
/// character.
let CHANNEL_ESCAPES: [Character: Character] = [
    "\"": "\"",
    "'": "'",
    "\\": "\\",
    "/": "/",
    "n": "\n",
    "r": "\r",
    "t": "\t",
    "b": "\u{08}",
    "f": "\u{0C}",
]

/// `(string, next)` for a `'…'` or `"…"` value; nil means malformed.
///
/// BOTH quote styles, because which one a model reaches for is a property of its
/// training, not of our grammar — the documented gemma grammar uses `<|"|>`, the
/// 2026-08-04 run mixed in JSON double quotes, and the 2026-08-06 run emitted
/// `call:kernel_state{type: 'send', target_id: 'kernel', …}` in single quotes.
/// That last one was a fully correct intent and the parser threw it away, which
/// ended the run: the model had nothing left to fix.
///
/// ONLY the matching quote closes. So an opener of one style closed by the other
/// never terminates, runs to the end of the body, and is reported malformed —
/// which is right, because a mismatched pair is a real error and not a shape any
/// model actually means.
func parseQuotedChannelString(_ chars: [Character], _ from: Int) -> (value: String, next: Int)? {
    let quote = chars[from]
    var out = String()
    var i = from + 1
    while i < chars.count {
        let c = chars[i]
        if c == "\\", i + 1 < chars.count {
            let next = chars[i + 1]
            if next == "u", i + 5 < chars.count, let scalar = hexScalar(chars, i + 2) {
                out.unicodeScalars.append(scalar)
                i += 6
                continue
            }
            // An unknown escape keeps BOTH characters rather than guessing: a
            // Windows path in a program body (`\d`) must survive verbatim.
            if let decoded = CHANNEL_ESCAPES[next] {
                out.append(decoded)
            } else {
                out.append("\\")
                out.append(next)
            }
            i += 2
            continue
        }
        if c == quote { return (out, i + 1) }
        out.append(c)
        i += 1
    }
    return nil
}

/// `(value, next)` for one channel value; nil means malformed.
func parseChannelValue(_ chars: [Character], _ from: Int) -> (value: JSON, next: Int)? {
    var i = skipWS(chars, from)
    guard i < chars.count else { return nil }
    if matchesAt(chars, CHANNEL_STR_CHARS, i) {
        let bodyStart = i + CHANNEL_STR_CHARS.count
        guard let close = findChars(chars, CHANNEL_STR_CHARS, from: bodyStart) else { return nil }
        // No escapes inside: the token cannot be forged, so the content is taken
        // verbatim — commas and braces included.
        return (.string(String(chars[bodyStart..<close])), close + CHANNEL_STR_CHARS.count)
    }
    if chars[i] == "\"" || chars[i] == "'" {
        // MIXED SYNTAX — the envelope is the channel one, the strings are
        // ordinary quoted text. Measured 2026-08-04: gemma-4-12B-it emitted
        //   call:kernel_state.update_agent{"id":"assets","ingress_rule":"allow_all"}
        // Without this branch the bare-token path returns `"assets"` WITH its
        // quote characters, `id` never arrives, and the call comes back
        // `{"error": "update_agent: id required"}` — which is what killed all
        // three posture changes in that run.
        guard let quoted = parseQuotedChannelString(chars, i) else { return nil }
        return (.string(quoted.value), quoted.next)
    }
    if chars[i] == "{" {
        guard let nested = parseChannelObject(chars, i) else { return nil }
        return (.object(nested.value), nested.next)
    }
    if chars[i] == "[" {
        var out: [JSON] = []
        i = skipWS(chars, i + 1)
        if i < chars.count, chars[i] == "]" { return (.array(out), i + 1) }
        while true {
            guard let element = parseChannelValue(chars, i) else { return nil }
            out.append(element.value)
            i = skipWS(chars, element.next)
            if i < chars.count, chars[i] == "," {
                i += 1
                continue
            }
            if i < chars.count, chars[i] == "]" { return (.array(out), i + 1) }
            return nil
        }
    }
    // A BARE token, ending at the first structural character. A QUOTED string is
    // allowed to contain `,` `}` `]` (see above); this reader stops at them.
    var end = i
    while end < chars.count, chars[end] != ",", chars[end] != "}", chars[end] != "]" {
        end += 1
    }
    let raw = String(chars[i..<end]).trimmingCharacters(in: .whitespacesAndNewlines)
    if raw.isEmpty { return nil }
    return (channelScalar(raw), end)
}

/// `(dict, next)` for a `{k:v,k:v}` body; nil means malformed.
func parseChannelObject(
    _ chars: [Character], _ from: Int
) -> (value: OrderedDictionary<String, JSON>, next: Int)? {
    var i = skipWS(chars, from)
    guard i < chars.count, chars[i] == "{" else { return nil }
    i = skipWS(chars, i + 1)
    var out: OrderedDictionary<String, JSON> = [:]
    if i < chars.count, chars[i] == "}" { return (out, i + 1) }
    while true {
        i = skipWS(chars, i)
        let key: String
        var afterKey: Int
        if i < chars.count, chars[i] == "\"" || chars[i] == "'" {
            // Keys are bare in the documented grammar, but a model mixing JSON
            // quotes them — in EITHER style, and the quotes must come OFF:
            // carrying `"id"` as a literal key name is what killed three posture
            // changes on 2026-08-04. Parsed with the same reader as a value so a
            // key is allowed to contain `:` or `,`; scanning bare to the first
            // structural character would split such a key in half and lose the
            // rest.
            guard let quoted = parseQuotedChannelString(chars, i) else { return nil }
            key = quoted.value
            afterKey = skipWS(chars, quoted.next)
        } else {
            var k = i
            while k < chars.count, chars[k] != ":", chars[k] != ",", chars[k] != "}" { k += 1 }
            key = String(chars[i..<k]).trimmingCharacters(in: .whitespacesAndNewlines)
            afterKey = k
        }
        guard !key.isEmpty, afterKey < chars.count, chars[afterKey] == ":" else { return nil }
        guard let value = parseChannelValue(chars, afterKey + 1) else { return nil }
        out[key] = value.value
        i = skipWS(chars, value.next)
        if i < chars.count, chars[i] == "," {
            i += 1
            continue
        }
        if i < chars.count, chars[i] == "}" { return (out, i + 1) }
        return nil
    }
}

/// A bare token's type: `true` / `false` / `null` / `none` (case-insensitively),
/// then a number, else the text itself.
///
/// The numeric reader is deliberately NARROWER than python's `int()` / `float()`
/// pair, in the one direction that matters — it never turns text into a number
/// python would have kept as text. Swift's `Double(_:)` understands hex floats,
/// so an unguarded attempt would read a bare `0x10` as `16` where python keeps
/// the string; and `inf` / `nan` parse in both languages but have no JSON form,
/// so a call carrying one could not be serialized to the agent it addresses.
private func channelScalar(_ raw: String) -> JSON {
    switch raw.lowercased() {
    case "true": return .bool(true)
    case "false": return .bool(false)
    case "null", "none": return .null
    default: break
    }
    guard looksNumeric(raw) else { return .string(raw) }
    // Integer before double, as python does, so `3` round-trips as `3` and not
    // as `3.0`.
    if let i = Int64(raw) { return .integer(i) }
    if let d = Double(raw) { return .double(d) }
    return .string(raw)
}

/// Is every character one a decimal JSON number may use, with at least one
/// digit among them? Rejects hex, `inf`, `nan` and python's `1_000`.
private func looksNumeric(_ raw: String) -> Bool {
    var sawDigit = false
    for c in raw {
        if c.isNumber, c.isASCII {
            sawDigit = true
            continue
        }
        guard c == "+" || c == "-" || c == "." || c == "e" || c == "E" else { return false }
    }
    return sawDigit
}

/// The scalar written by exactly four hex digits at `at`, or nil.
///
/// Python's `chr(int(s[i+2:i+6], 16))` inside a `try`, and the `nil` here plays
/// the part of its `ValueError`: the caller then falls back to the escape table,
/// so `\uZZZZ` survives as the literal text the model typed. One divergence, and
/// it is a language limit rather than a choice — python's `chr` accepts a LONE
/// SURROGATE and Swift's `String` cannot hold one, so `\ud800` alone survives
/// verbatim here where python would have produced an unpaired surrogate (which
/// its own JSON encoder then refuses).
private func hexScalar(_ chars: [Character], _ at: Int) -> Unicode.Scalar? {
    var value: UInt32 = 0
    for k in 0..<4 {
        guard let digit = chars[at + k].hexDigitValue else { return nil }
        value = (value << 4) | UInt32(digit)
    }
    return Unicode.Scalar(value)
}
