// Character-array scanning primitives, shared by every envelope's reader.
//
// SWIFT-SPECIFIC and load-bearing: python indexes strings by integer, so a
// hand-port that juggles `String.Index` grows a SECOND index space and the
// step-over-the-OPENER-only rule in `ToolScan` drifts silently. Everything in
// this module indexes `Array(text)` (`[Character]`) so the arithmetic matches
// python's string indices exactly. Byte offsets appear in exactly one place in
// the whole module — `parseOne`'s `JSON.parsePrefix` call.
//
// These three stand in for python's `str.find(sub, i)`,
// `str.startswith(sub, i)` and `_skip_ws`. There is no regex in the port:
// python's `re` is stdlib and ours would be a dependency for a handful of
// literal characters, while the grammars below are hand-written scanners in
// both runtimes anyway.

import Foundation

/// Index of `needle` in `haystack` at or after `from`, or nil.
///
/// Python's `str.find(sub, i)`, minus the `-1` sentinel.
func findChars(_ haystack: [Character], _ needle: [Character], from: Int) -> Int? {
    if needle.isEmpty || from >= haystack.count { return nil }
    let last = haystack.count - needle.count
    if last < from { return nil }
    var start = from
    while start <= last {
        var k = 0
        while k < needle.count, haystack[start + k] == needle[k] { k += 1 }
        if k == needle.count { return start }
        start += 1
    }
    return nil
}

/// Does `needle` sit exactly at `at`? Python's `str.startswith(sub, i)`.
func matchesAt(_ haystack: [Character], _ needle: [Character], _ at: Int) -> Bool {
    guard at >= 0, at + needle.count <= haystack.count else { return false }
    for k in 0..<needle.count where haystack[at + k] != needle[k] { return false }
    return true
}

/// Advance past whitespace. Python's `_skip_ws`.
func skipWS(_ chars: [Character], _ from: Int) -> Int {
    var i = from
    while i < chars.count, chars[i].isWhitespace { i += 1 }
    return i
}
