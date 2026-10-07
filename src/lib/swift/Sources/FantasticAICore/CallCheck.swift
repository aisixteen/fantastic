// The CALL CHECKER — what a model wrote as a call that is not one (owner,
// 2026-10-07). Python `ai_core/call_check.py`, report for report.
//
// A model may invent any call syntax — a `<tool_call>` wrapper, `<function=call
// name="send">`, `<send>` as a tag, a JSON object, `<parameter=x>` arguments — and
// none of it opens a `<call` envelope, so the scanner finds nothing and the turn
// reads as call-less (measured 2026-10-07, E59 `calc` on qwen3.8-27b: 18 of 25
// replies were `<tool_call><function=call name="send">…</call>`, each answered with
// the call-less reminder, the same shape repeated until the chain gave up).
//
// The foreign syntaxes cannot be listed, so this checks the other way round: OUR
// grammar and OUR vocabulary are closed — `<call name="N">`, one child tag per
// argument, `</call>`; the call names (`TOOLS`) and the routing tags are known — and
// whatever markup carries that vocabulary OUTSIDE every `<call` envelope is a call
// written wrong. Each is reported like a compiler error: the line, a caret under the
// first token that departs from the grammar, and the form to write instead, built
// from the name and arguments the model used.
//
// Nothing here runs a call: one grammar, one parser. Everything is indexed in
// UNICODE SCALARS — python's code points — so lines, columns and carets match.
// Python's regexes are hand-walked here, each beside the pattern it mirrors.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public enum CallCheck {
    /// At most this many malformed calls are reported per answer, then a count.
    public static let MAX_REPORTS = 3
    /// Chars of a quoted line.
    static let SHOWN = 160

    /// The argument tags a call carries that are ours by name: the routing fields
    /// and the two every payload has.
    static func isArgTag(_ name: String) -> Bool {
        ROUTING.contains(name) || name == "type" || name == "bytes"
    }

    enum Kind { case opener, close, arg, param, other, json }

    struct Tok {
        var kind: Kind
        let text: String
        let start: Int
        let end: Int
        var name: String? = nil
        var arg: String? = nil
        let closing: Bool
    }

    struct Call {
        var toks: [Tok] = []
        var wrappers: [Tok] = []
    }

    typealias S = [Unicode.Scalar]

    static func str(_ s: ArraySlice<Unicode.Scalar>) -> String {
        String(String.UnicodeScalarView(s))
    }

    /// python `\w` (a str pattern): a Unicode letter or digit, or `_`.
    static func isWord(_ c: Unicode.Scalar) -> Bool {
        c == "_" || c.properties.isAlphabetic || c.properties.numericType != nil
    }

    static func isIdentStart(_ c: Unicode.Scalar) -> Bool {
        (c >= "A" && c <= "Z") || (c >= "a" && c <= "z") || c == "_"
    }

    /// `["']?([A-Za-z_]\w*)` at `i`: the identifier, or nil.
    static func ident(_ s: S, _ i0: Int) -> String? {
        var i = i0
        if i < s.count, s[i] == "\"" || s[i] == "'" { i += 1 }
        guard i < s.count, isIdentStart(s[i]) else { return nil }
        var j = i + 1
        while j < s.count, isWord(s[j]) { j += 1 }
        return str(s[i..<j])
    }

    static func skipSpace(_ s: S, _ i0: Int) -> Int {
        var i = i0
        while i < s.count, pyIsSpace(s[i]) { i += 1 }
        return i
    }

    /// python `_NAMED.search(rest)`: `\bname\s*=\s*["']?([A-Za-z_]\w*)`.
    static func named(_ rest: S) -> String? {
        let key = Array("name".unicodeScalars)
        var i = 0
        while i + key.count <= rest.count {
            if Array(rest[i..<i + key.count]) == key, i == 0 || !isWord(rest[i - 1]) {
                var j = skipSpace(rest, i + key.count)
                if j < rest.count, rest[j] == "=" {
                    j = skipSpace(rest, j + 1)
                    if let id = ident(rest, j) { return id }
                }
            }
            i += 1
        }
        return nil
    }

    /// python `_EQ.match(rest)`: `^=\s*["']?([A-Za-z_]\w*)`.
    static func eq(_ rest: S) -> String? {
        guard rest.first == "=" else { return nil }
        return ident(rest, skipSpace(rest, 1))
    }

    /// The answer with every `<call` envelope blanked — those are the scanner's
    /// (dispatched, or reported as unparsable); newlines kept so lines still count.
    static func mask(_ text: S) -> S {
        var out = text
        for (start, _, end) in envelopes(text) {
            let stop = end.map { $0 + CLOSE.count } ?? text.count
            for i in start..<stop where out[i] != "\n" { out[i] = " " }
        }
        return out
    }

    /// python `_TAG.finditer`: `<(/?)([^\s<>/=]*)([^<>\n]{0,300}?)(/?)>`, then
    /// classified.
    static func tags(_ s: S) -> [Tok] {
        var out: [Tok] = []
        var i = 0
        while i < s.count {
            guard s[i] == "<" else { i += 1; continue }
            var j = i + 1
            let slash = j < s.count && s[j] == "/"
            if slash { j += 1 }
            var k = j
            while k < s.count, !pyIsSpace(s[k]), !"<>/=".unicodeScalars.contains(s[k]) { k += 1 }
            // the first `>` with no `<` or newline before it
            var e = k
            while e < s.count, s[e] != ">", s[e] != "<", s[e] != "\n" { e += 1 }
            guard e < s.count, s[e] == ">" else { i += 1; continue }
            let selfClose = e > k && s[e - 1] == "/"
            let rest = Array(s[k..<(selfClose ? e - 1 : e)])
            guard rest.count <= 300 else { i += 1; continue }
            out.append(classify(s, start: i, end: e + 1, slash: slash, head: str(s[j..<k]), rest: rest))
            i = e + 1
        }
        return out
    }

    static func classify(_ s: S, start: Int, end: Int, slash: Bool, head: String, rest: S) -> Tok {
        var t = Tok(kind: .other, text: str(s[start..<end]), start: start, end: end, closing: slash)
        let nm = named(rest)
        let eqv = eq(rest)
        if head == "call" {
            t.kind = slash ? .close : .opener
            t.name = nm
        } else if TOOLS.contains(head) {
            t.kind = slash ? .close : .opener
            t.name = head
        } else if !slash, let nm, TOOLS.contains(nm) {
            t.kind = .opener
            t.name = nm
        } else if !slash, let eqv, TOOLS.contains(eqv) {
            t.kind = .opener
            t.name = eqv
        } else if !slash, let eqv, isArgTag(eqv) {
            t.kind = .param
            t.arg = eqv
        } else if isArgTag(head) {
            t.kind = .arg
            t.arg = head
        }
        return t
    }

    /// python `_JSON.finditer`:
    /// `\{\s*"(?:name|tool|tool_name|function)"\s*:\s*"(send|done|await|inquiry_answer)"`.
    static func jsonCalls(_ s: S) -> [Tok] {
        var out: [Tok] = []
        var i = 0
        while i < s.count {
            guard s[i] == "{" else { i += 1; continue }
            let j = skipSpace(s, i + 1)
            var hit: (end: Int, name: String)? = nil
            if j < s.count, s[j] == "\"" {
                for key in ["name", "tool", "tool_name", "function"] {
                    let k = Array("\(key)\"".unicodeScalars)
                    guard j + 1 + k.count <= s.count, Array(s[(j + 1)..<(j + 1 + k.count)]) == k
                    else { continue }
                    var p = skipSpace(s, j + 1 + k.count)
                    guard p < s.count, s[p] == ":" else { continue }
                    p = skipSpace(s, p + 1)
                    guard p < s.count, s[p] == "\"" else { continue }
                    for tool in TOOLS {
                        let tq = Array("\(tool)\"".unicodeScalars)
                        if p + 1 + tq.count <= s.count, Array(s[(p + 1)..<(p + 1 + tq.count)]) == tq {
                            hit = (p + 1 + tq.count, tool)
                            break
                        }
                    }
                    if hit != nil { break }
                }
            }
            if let hit {
                out.append(
                    Tok(kind: .json, text: str(s[i..<hit.end]), start: i, end: hit.end, name: hit.name, closing: false))
                i = hit.end
            } else {
                i += 1
            }
        }
        return out
    }

    static func newlines(_ s: S, _ upto: Int) -> Int {
        s[0..<upto].reduce(0) { $0 + ($1 == "\n" ? 1 : 0) }
    }

    /// The call-like runs: each starts at a call-like token and ends at a closing
    /// `</call>` / `</<call name>>` (a JSON object is one run). Foreign tags right
    /// before a run's first token — on its line or the one above — are its wrappers.
    static func calls(_ masked: S, _ toks: [Tok]) -> [Call] {
        var out: [Call] = []
        var cur: Call? = nil
        var pending: [Tok] = []
        for t in toks {
            if t.kind == .other {
                if cur != nil { cur!.toks.append(t) } else { pending.append(t) }
                continue
            }
            if cur == nil {
                let firstLine = newlines(masked, t.start)
                cur = Call(
                    toks: [],
                    wrappers: pending.filter {
                        !$0.closing && firstLine - newlines(masked, $0.start) <= 1
                    })
                pending = []
            }
            cur!.toks.append(t)
            if t.kind == .json || t.kind == .close {
                out.append(cur!)
                cur = nil
            }
        }
        if let cur { out.append(cur) }
        return out.filter { $0.toks.contains { $0.kind != .other } }
    }

    static func find(_ s: S, _ needle: S, from: Int) -> Int? {
        guard !needle.isEmpty, needle.count <= s.count else { return nil }
        var i = from
        while i + needle.count <= s.count {
            if s[i] == needle[0], Array(s[i..<i + needle.count]) == needle { return i }
            i += 1
        }
        return nil
    }

    /// The text between an argument's open tag and its close, on the masked answer.
    static func value(_ masked: S, _ tok: Tok, _ closer: String) -> String? {
        guard let end = find(masked, Array(closer.unicodeScalars), from: tok.end) else { return nil }
        let v = Array(pyStrip(str(masked[tok.end..<end])).unicodeScalars)
        return v.count <= 200 ? str(v[...]) : str(v[0..<200]) + "…"
    }

    /// python `re.compile(r"</[^<>\n]{0,40}>").search(masked, t.end)` — its start.
    static func nextClose(_ s: S, from: Int) -> Int? {
        var i = from
        while i + 1 < s.count {
            if s[i] == "<", s[i + 1] == "/" {
                var e = i + 2
                while e < s.count, e - (i + 2) <= 40, s[e] != "<", s[e] != ">", s[e] != "\n" { e += 1 }
                if e < s.count, s[e] == ">", e - (i + 2) <= 40 { return i }
            }
            i += 1
        }
        return nil
    }

    /// The arguments of a JSON call object, flattened one level (a `payload` /
    /// `arguments` / `parameters` object's keys become tags of the call).
    static func jsonArgs(_ masked: S, _ tok: Tok) -> OrderedDictionary<String, JSON> {
        guard let data = str(masked[tok.start...]).data(using: .utf8),
            let (value, _) = try? JSON.parsePrefix(data), case .object(let obj) = value
        else { return [:] }
        var args: OrderedDictionary<String, JSON> = [:]
        for (k, v) in obj {
            if ["name", "tool", "tool_name", "function"].contains(k) { continue }
            if case .object(let inner) = v {
                for (k2, v2) in inner {
                    if case .object(let deep) = v2 {
                        for (k3, v3) in deep { args[k3] = v3 }
                    } else {
                        args[k2] = v2
                    }
                }
            } else {
                args[k] = v
            }
        }
        return args
    }

    static func expected(_ name: String, _ args: [(String, JSON)]) -> String {
        if args.isEmpty { return "<call name=\"\(name)\"></call>" }
        var lines = ["<call name=\"\(name)\">"]
        for (k, v) in args {
            if case .array(let xs) = v {
                lines.append("<\(k)>" + xs.map { "<item>\(pyStr($0))</item>" }.joined() + "</\(k)>")
            } else {
                lines.append("<\(k)>\(pyStr(v))</\(k)>")
            }
        }
        lines.append("</call>")
        return lines.joined(separator: "\n")
    }

    /// (line, col, the quoted line, its caret) — 1-based, the line windowed to
    /// `SHOWN` chars around the token, defanged so the quote cannot arm the scanner
    /// (`defang` swaps one char for one, so the caret still lines up).
    static func place(_ text: S, _ tok: Tok) -> (Int, Int, String, String) {
        var lineStart = tok.start
        while lineStart > 0, text[lineStart - 1] != "\n" { lineStart -= 1 }
        var lineEnd = tok.start
        while lineEnd < text.count, text[lineEnd] != "\n" { lineEnd += 1 }
        let line = Array(text[lineStart..<lineEnd])
        let col = tok.start - lineStart
        let lo = max(0, col - 40)
        let shown = Array(line[min(lo, line.count)..<min(lo + SHOWN, line.count)])
        let lead = lo > 0 ? "…" : ""
        let width = max(1, min(tok.text.unicodeScalars.count, shown.count - (col - lo)))
        let caret =
            String(repeating: " ", count: lead.unicodeScalars.count + col - lo) + "^"
            + String(repeating: "~", count: width - 1)
        return (newlines(text, tok.start) + 1, col + 1, lead + defang(str(shown[...])), caret)
    }

    /// The name of a bare opening tag (`<query>`, no attributes) that is none of
    /// ours — inside a call run, an argument of the verb. Python `_plain`.
    static func plain(_ t: Tok) -> String? {
        guard t.kind == .other, t.text.hasPrefix("<"), t.text.hasSuffix(">") else { return nil }
        let inner = Array(t.text.unicodeScalars.dropFirst().dropLast())
        guard let first = inner.first, isIdentStart(first),
            inner.dropFirst().allSatisfy({ isWord($0) || $0 == "-" })
        else { return nil }
        let name = str(inner[...])
        return name == "item" ? nil : name
    }

    static func report(_ text: S, _ masked: S, _ call: Call) -> String {
        let first = call.toks.first { $0.kind != .other }!
        let name = call.toks.compactMap(\.name).first ?? "send"
        var args: [(String, JSON)] = []
        var notes: [String] = []
        let why: String
        switch first.kind {
        case .json:
            args = jsonArgs(masked, first).map { ($0.key, $0.value) }
            why = "calls are tags, not JSON"
        case .arg, .param: why = "an argument outside a call — open the call above it"
        case .close: why = "a close with no call open"
        default: why = "not a call tag"
        }
        // EVERY ARGUMENT THE MODEL WROTE carries into the form to write — the verb's
        // own (`<query>`, `<ids>`) as much as ours by name. Measured 2026-10-07
        // (misleading_fold:rust): a grep scoped by `<ids>` lost its `<` and was
        // reported with only target/title/type; the model rewrote it unscoped. A tag
        // nested in an argument (an `<item>` of `<ids>`) is part of that value.
        var after = 0
        func take(_ t: Tok, _ a: String) {
            let closer = "</\(a)>"
            guard let v = value(masked, t, closer) else { return }
            if let at = find(masked, Array(closer.unicodeScalars), from: t.end) {
                after = at + closer.unicodeScalars.count
            }
            args.append((a, .string(v)))
        }
        for t in call.toks {
            if t.start < after { continue }
            switch t.kind {
            case .arg where !t.closing:
                if let a = t.arg { take(t, a) }
            case .other where plain(t) != nil:
                take(t, plain(t)!)
            case .param:
                if let a = t.arg, let close = nextClose(masked, from: t.end) {
                    let v = Array(pyStrip(str(masked[t.end..<close])).unicodeScalars).prefix(200)
                    args.append((a, .string(str(v))))
                    notes.append(
                        "`\(defang(t.text))` — one child tag per argument: <\(a)>…</\(a)>")
                }
            case .close where t.text != CLOSE_TEXT:
                notes.append("`\(defang(t.text))` — close with </call>")
            default: break
            }
        }
        notes =
            call.wrappers.map {
                "line \(newlines(text, $0.start) + 1) `\(defang($0.text))` — drop the wrapper"
            } + notes
        let (line, col, quoted, caret) = place(text, first)
        var out = [
            "[malformed call — nothing ran] line \(line), col \(col):",
            "  \(quoted)",
            "  \(caret) \(why)",
        ]
        if first.kind == .close, call.toks.filter({ $0.kind != .other }).count == 1 {
            out[out.count - 1] += " — delete it"
        } else {
            out += ["write it as:", expected(name, args)]
        }
        if !notes.isEmpty { out.append("also: " + notes.joined(separator: "; ") + ".") }
        out.append("A call is <call name=\"…\">, one child tag per argument, closed by </call>.")
        return out.joined(separator: "\n")
    }

    static let CLOSE_TEXT = "</call>"

    /// One report per call the answer wrote outside every `<call` envelope, in
    /// document order (at most `MAX_REPORTS`, then a count); empty when there is
    /// none. `ran` is how many of the answer's calls were dispatched — said in each
    /// report, so the model knows its well-formed calls are not to be written again.
    public static func check(_ answer: String, ran: Int) -> [String] {
        let text = Array(answer.unicodeScalars)
        let masked = mask(text)
        let toks = (tags(masked) + jsonCalls(masked)).sorted { $0.start < $1.start }
        let found = calls(masked, toks)
        var reports = found.prefix(MAX_REPORTS).map { report(text, masked, $0) }
        if found.count > MAX_REPORTS {
            reports[reports.count - 1] += "\n(\(found.count - MAX_REPORTS) more malformed calls not shown.)"
        }
        if ran > 0 {
            let tail =
                ran == 1
                ? "The other call of this answer ran — its reply is in this batch; do not write it again."
                : "The other \(ran) calls of this answer ran — their replies are in this batch; do not "
                    + "write them again."
            reports = reports.map { $0 + "\n" + tail }
        }
        return reports
    }
}
