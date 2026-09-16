// Scalar-array scanning primitives, shared by every envelope's reader.
//
// SWIFT-SPECIFIC and load-bearing: python indexes a str by CODE POINT, so the
// only array whose arithmetic matches python's string indices is
// `Array(text.unicodeScalars)`. `Array(text)` (`[Character]`) does not: a CRLF is
// one Character and two code points, `é` written NFD is one Character and two
// code points, and two Characters compare EQUAL when they are canonically
// equivalent — so a grapheme-indexed port reported different excerpt lengths,
// closed a tag an NFC/NFD mismatch should have left open, and dispatched a tag
// python calls prose. Byte offsets appear nowhere in this module.
//
// These three stand in for python's `str.find(sub, i)`,
// `str.startswith(sub, i)` and `lstrip()`. There is no regex in the port:
// python's `re` is stdlib and ours would be a dependency for a handful of
// literal characters, while the grammars below are hand-written scanners in
// both runtimes anyway.

import FantasticJSON
import Foundation

/// Index of `needle` in `haystack` at or after `from`, or nil.
///
/// Python's `str.find(sub, i)`, minus the `-1` sentinel.
func findScalars(_ haystack: [Unicode.Scalar], _ needle: [Unicode.Scalar], from: Int) -> Int? {
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
func matchesAt(_ haystack: [Unicode.Scalar], _ needle: [Unicode.Scalar], _ at: Int) -> Bool {
    guard at >= 0, at + needle.count <= haystack.count else { return false }
    for k in 0..<needle.count where haystack[at + k] != needle[k] { return false }
    return true
}

/// Advance past whitespace — python's `str.isspace()` set, which includes
/// U+001C..U+001F, so `<call\u{1c}name="send">` is the envelope python reads.
func skipWS(_ chars: [Unicode.Scalar], _ from: Int) -> Int {
    var i = from
    while i < chars.count, pyIsSpace(chars[i]) { i += 1 }
    return i
}

/// A scalar range back as text.
func scalarText(_ chars: [Unicode.Scalar], _ lo: Int, _ hi: Int) -> String {
    var view = String.UnicodeScalarView()
    var i = max(0, lo)
    let end = min(hi, chars.count)
    while i < end {
        view.append(chars[i])
        i += 1
    }
    return String(view)
}
