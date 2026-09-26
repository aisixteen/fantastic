// The on-disk form of kernel-written state: YAML, block style, one emitter config.
//
// A rule-for-rule port of python's
// `bundled_agents/loader/kernel_state/src/kernel_state/yaml_io.py`, which is
// canonical — and BYTE FOR BYTE with it (the acceptance harness diffs files).
// Pinned by the python-written fixture `tests/fixtures/agent.yaml` (a record);
// `YamlIOTests` loads it and re-writes it to identical bytes.
//
// Everything the kernel writes under `.fantastic` goes through `dump` here: the
// records (`agent.yaml`), the lock (`lock.yaml`) and the scheduler's sidecars
// (`schedules.yaml`, and `schedule_events.yaml` — a YAML STREAM, one `---`
// document per event, via `rowToDoc`).
//
// There is no message tape: the kernel keeps no conversation, and its execution
// rows live in its memory store (see `Kernel.flushExecutions`).
//
// THE EMITTER IS WRITTEN BY HAND, over the `JSON` type (OrderedDictionary key
// order kept). It is a port of libyaml's emitter state machine — the one PyYAML's
// `CSafeDumper` drives — restricted to what `dump` ever asks of it: block style,
// indent 2, a list under a key NOT indented, unbounded width (no folding), no
// anchors, an EMPTY map/list as the flow `{}` / `[]`, `...` only where libyaml
// writes one (after a keep-chomped `|+` literal: a text whose last line break
// follows another break, e.g. U+2028 + `\n`; `\n\n` itself is quoted). Exact bytes are
// easier to hold by hand than through a wrapper's own representer (Yams formats
// floats and picks string styles its own way). PARSING is Yams (libyaml's
// parser, pinned exact in Package.swift): each document is composed to a node
// tree and a PLAIN scalar is typed by python's YAML 1.1 resolver, ported below —
// so what swift reads is what `CSafeLoader` reads.
//
// STRING STYLE, per string (python `_represent_str`):
//   - contains `\n`, not ending in `\n\n` and not exactly `\n` → request LITERAL
//     `|` (the emitter downgrades to double-quoted when a literal cannot hold it);
//   - contains `\n` and ends in `\n\n` (or is `\n`) → DOUBLE-QUOTED;
//   - a YAML-1.2-core number lookalike (`0o17`, `1e3`, `5.`) → SINGLE-QUOTED;
//   - otherwise plain when the 1.1 resolver reads it back as the same string,
//     else single-quoted (`'1'`, `'true'`, `'null'`, `''`, `'yes'`).
// Non-BMP characters are escaped `\UXXXXXXXX` inside a double-quoted scalar.
// Scalars: int decimal, float python-`repr` (`1.5`, `.inf`, `.nan`), bool
// `true`/`false`, null `null` (kept, not omitted).

import FantasticJSON
import Foundation
import OrderedCollections
import Yams

/// A YAML document this module cannot read as written.
public struct YamlIOError: Error, CustomStringConvertible {
    public let description: String
    init(_ s: String) { description = s }
}

public enum YamlIO {
    public static let separator = "---\n"

    // MARK: - write

    /// One YAML document (no `---`, no `...`) by the rules above.
    public static func dump(_ value: JSON) -> String {
        var e = Emitter()
        e.node(value, mappingContext: false, simpleKey: false)
        e.writeIndent()  // the document end
        if e.openEnded {
            // libyaml closes a document that holds a keep-chomped (`|+`) block
            // scalar with `...` — python writes it, so this does too.
            e.writeIndicator("...", true, false, false)
            e.writeIndent()
        }
        return e.out
    }

    /// One entry of a YAML stream as one document: `---\n` + the block mapping.
    public static func rowToDoc(_ row: Row) -> String {
        separator + dump(.object(row))
    }

    // MARK: - read

    /// One YAML document back. Throws on bad input.
    public static func load(_ text: String) throws -> JSON {
        guard let node = try Yams.compose(yaml: text, .basic) else { return .null }
        return try toJSON(node)
    }

    /// Every document of a stream, in order (`yaml.load_all`).
    public static func loadAll(_ text: String) throws -> [JSON] {
        var out: [JSON] = []
        _ = try splitDocuments([text]) { doc, _ in out.append(try load(doc)) }
        return out
    }

    // MARK: - the stream splitter

    /// Cut a chunked stream into its documents at the `---` separator lines,
    /// handing each document's text and its first line number (1-based) to
    /// `onDoc` as soon as it is complete. A `---` line never occurs inside a
    /// document this module writes: every column-0 line is a key or a `- ` item,
    /// and a `---` key is quoted. Returns the stream's last character and its
    /// length in characters (the cut check needs the tail).
    private static func splitDocuments<S: Sequence>(
        _ chunks: S, _ onDoc: (String, Int) throws -> Void
    ) throws -> (last: Character?, size: Int) where S.Element == String {
        var pending = ""
        var doc = ""
        var inDoc = false
        var docLine = 1
        var lineNo = 0
        var last: Character? = nil
        var size = 0
        func line(_ l: Substring) throws {
            lineNo += 1
            if l == "---\n" {
                if inDoc { try onDoc(doc, docLine) }
                doc = ""
                inDoc = true
                docLine = lineNo + 1
            } else {
                if !inDoc { docLine = lineNo }
                doc.append(contentsOf: l)
                inDoc = true
            }
        }
        for chunk in chunks where !chunk.isEmpty {
            size += chunk.unicodeScalars.count
            last = chunk.unicodeScalars.last.map(Character.init)
            pending += chunk
            var start = pending.startIndex
            while let nl = pending[start...].firstIndex(of: "\n") {
                let end = pending.index(after: nl)
                try line(pending[start..<end])
                start = end
            }
            pending = String(pending[start...])
        }
        if !pending.isEmpty {
            if !inDoc { docLine = lineNo + 1 }
            doc += pending
            inDoc = true
        }
        if inDoc { try onDoc(doc, docLine) }
        return (last, size)
    }

    // MARK: - node → JSON (python's CSafeLoader typing)

    private static func toJSON(_ node: Node) throws -> JSON {
        switch node {
        case .scalar(let s):
            return s.style == .plain ? resolvePlain(s.string) : .string(s.string)
        case .mapping(let m):
            var d = OrderedDictionary<String, JSON>()
            for (k, v) in m {
                let key: String
                if case .scalar(let ks) = k { key = ks.string } else { key = "\(k)" }
                d[key] = try toJSON(v)
            }
            return .object(d)
        case .sequence(let q):
            return .array(try q.map(toJSON))
        case .alias:
            throw YamlIOError("yaml: an alias is never written by this kernel")
        }
    }

    /// A PLAIN scalar typed by the YAML 1.1 resolver + PyYAML's constructors. A
    /// timestamp stays a string (python reads a date, and writes it back as the
    /// same quoted string — same bytes).
    static func resolvePlain(_ s: String) -> JSON {
        if Resolve11.null.matches(s) { return .null }
        if Resolve11.bool.matches(s) {
            return .bool(["yes", "Yes", "YES", "true", "True", "TRUE", "on", "On", "ON"].contains(s))
        }
        if Resolve11.int.matches(s) { return constructInt(s) }
        if Resolve11.float.matches(s) { return .double(constructFloat(s)) }
        return .string(s)
    }

    private static func constructInt(_ raw: String) -> JSON {
        var v = raw.replacingOccurrences(of: "_", with: "")
        var sign: Int64 = 1
        if v.first == "-" { sign = -1 }
        if v.first == "-" || v.first == "+" { v.removeFirst() }
        let mag: Int64?
        if v == "0" {
            mag = 0
        } else if v.hasPrefix("0b") {
            mag = Int64(v.dropFirst(2), radix: 2)
        } else if v.hasPrefix("0x") {
            mag = Int64(v.dropFirst(2), radix: 16)
        } else if v.first == "0" {
            mag = Int64(v, radix: 8)
        } else if v.contains(":") {
            var total: Int64 = 0
            for part in v.split(separator: ":") { total = total * 60 + (Int64(part) ?? 0) }
            mag = total
        } else {
            mag = Int64(v)
        }
        if let m = mag { return .integer(sign * m) }
        return .double(Double(sign) * (Double(v) ?? .nan))
    }

    private static func constructFloat(_ raw: String) -> Double {
        var v = raw.replacingOccurrences(of: "_", with: "").lowercased()
        var sign = 1.0
        if v.first == "-" { sign = -1 }
        if v.first == "-" || v.first == "+" { v.removeFirst() }
        if v == ".inf" { return sign * .infinity }
        if v == ".nan" { return .nan }
        if v.contains(":") {
            let parts = v.split(separator: ":").map { Double($0) ?? 0 }
            return sign * parts.reduce(0) { $0 * 60 + $1 }
        }
        return sign * (Double(v) ?? .nan)
    }
}

// MARK: - the resolvers

/// A full-match regex (python `re.fullmatch` / `^…$` on a newline-free string).
struct FullRegex: @unchecked Sendable {
    let re: NSRegularExpression
    init(_ pattern: String) { re = try! NSRegularExpression(pattern: "^(?:\(pattern))$") }
    func matches(_ s: String) -> Bool {
        re.firstMatch(in: s, range: NSRange(s.startIndex..., in: s)) != nil
    }
}

/// PyYAML's YAML 1.1 implicit resolvers (`yaml/resolver.py`), verbatim.
enum Resolve11 {
    static let bool = FullRegex(
        "yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF")
    static let float = FullRegex(
        "[-+]?(?:[0-9][0-9_]*)\\.[0-9_]*(?:[eE][-+][0-9]+)?"
            + "|\\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?"
            + "|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\\.[0-9_]*"
            + "|[-+]?\\.(?:inf|Inf|INF)"
            + "|\\.(?:nan|NaN|NAN)")
    static let int = FullRegex(
        "[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+"
            + "|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+")
    static let merge = FullRegex("<<")
    static let null = FullRegex("~|null|Null|NULL|")
    static let timestamp = FullRegex(
        "[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]"
            + "|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \\t]+)[0-9][0-9]?"
            + ":[0-9][0-9]:[0-9][0-9](?:\\.[0-9]*)?(?:[ \\t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?")
    static let value = FullRegex("=")
    static let yaml = FullRegex("!|&|\\*")
    /// Ours: a string the YAML 1.2 CORE schema would read as a number.
    static let core12Number = FullRegex(
        "[-+]?0o[0-7]+|[-+]?(\\.[0-9]+|[0-9]+(\\.[0-9]*)?)([eE][-+]?[0-9]+)?")

    /// Does the 1.1 resolver read this plain scalar back as a string?
    static func isStr(_ s: String) -> Bool {
        !(bool.matches(s) || float.matches(s) || int.matches(s) || merge.matches(s)
            || null.matches(s) || timestamp.matches(s) || value.matches(s) || yaml.matches(s))
    }
}

// MARK: - the emitter (libyaml's, restricted)

private func isBreak(_ c: Unicode.Scalar) -> Bool {
    c == "\r" || c == "\n" || c.value == 0x85 || c.value == 0x2028 || c.value == 0x2029
}

private func isBlankZ(_ c: Unicode.Scalar) -> Bool {
    c == " " || c == "\t" || c == "\0" || isBreak(c)
}

/// libyaml `IS_PRINTABLE`.
private func isPrintable(_ c: Unicode.Scalar) -> Bool {
    let v = c.value
    return v == 0x0A || (0x20...0x7E).contains(v) || (0xA0...0xD7FF).contains(v)
        || ((0xE000...0xFFFD).contains(v) && v != 0xFEFF)
}

private struct ScalarAnalysis {
    var multiline = false
    var blockPlainAllowed = true
    var singleQuotedAllowed = true
    var blockAllowed = true
    var length = 0

    /// libyaml `yaml_emitter_analyze_scalar` (flow context never occurs here).
    init(_ s: String) {
        let u = Array(s.unicodeScalars)
        length = s.utf8.count
        if u.isEmpty {
            blockAllowed = false
            return
        }
        var blockIndicators = false
        var lineBreaks = false, special = false
        var leadingSpace = false, leadingBreak = false, trailingSpace = false, trailingBreak = false
        var breakSpace = false, spaceBreak = false
        var previousSpace = false, previousBreak = false
        if u.count >= 3
            && ((u[0] == "-" && u[1] == "-" && u[2] == "-") || (u[0] == "." && u[1] == "." && u[2] == "."))
        {
            blockIndicators = true
        }
        var preceded = true
        var followed = u.count < 2 || isBlankZ(u[1])
        let last = u.count - 1
        for i in 0..<u.count {
            let c = u[i]
            if i == 0 {
                if "#,[]{}&*!|>'\"%@`".unicodeScalars.contains(c) { blockIndicators = true }
                if (c == "?" || c == ":") && followed { blockIndicators = true }
                if c == "-" && followed { blockIndicators = true }
            } else {
                if c == ":" && followed { blockIndicators = true }
                if c == "#" && preceded { blockIndicators = true }
            }
            if !isPrintable(c) { special = true }
            if isBreak(c) { lineBreaks = true }
            if c == " " {
                if i == 0 { leadingSpace = true }
                if i == last { trailingSpace = true }
                if previousBreak { breakSpace = true }
                previousSpace = true
                previousBreak = false
            } else if isBreak(c) {
                if i == 0 { leadingBreak = true }
                if i == last { trailingBreak = true }
                if previousSpace { spaceBreak = true }
                previousSpace = false
                previousBreak = true
            } else {
                previousSpace = false
                previousBreak = false
            }
            preceded = isBlankZ(c)
            if i + 1 < u.count { followed = i + 2 >= u.count || isBlankZ(u[i + 2]) }
        }
        multiline = lineBreaks
        if leadingSpace || leadingBreak || trailingSpace || trailingBreak { blockPlainAllowed = false }
        if trailingSpace { blockAllowed = false }
        if breakSpace {
            blockPlainAllowed = false
            singleQuotedAllowed = false
        }
        if spaceBreak || special {
            blockPlainAllowed = false
            singleQuotedAllowed = false
            blockAllowed = false
        }
        if lineBreaks { blockPlainAllowed = false }
        if blockIndicators { blockPlainAllowed = false }
    }
}

private struct Emitter {
    enum Style { case plain, single, double, literal }

    var out = ""
    var column = 0
    var whitespace = true
    var indention = true
    var indent = -1
    var indents: [Int] = []
    /// The LAST literal written was `|+` (libyaml `open_ended = 2`).
    var openEnded = false

    mutating func put(_ c: Unicode.Scalar) {
        out.unicodeScalars.append(c)
        column += 1
    }

    mutating func put(_ s: String) { for c in s.unicodeScalars { put(c) } }

    mutating func putBreak() {
        out.unicodeScalars.append("\n")
        column = 0
    }

    mutating func writeBreak(_ c: Unicode.Scalar) {
        if c == "\n" {
            putBreak()
        } else {
            out.unicodeScalars.append(c)
            column = 0
        }
    }

    mutating func writeIndicator(_ s: String, _ needWhitespace: Bool, _ isWhitespace: Bool, _ isIndention: Bool) {
        if needWhitespace && !whitespace { put(" ") }
        put(s)
        whitespace = isWhitespace
        indention = indention && isIndention
    }

    mutating func writeIndent() {
        let ind = max(indent, 0)
        if !indention || column > ind || (column == ind && !whitespace) { putBreak() }
        while column < ind { put(" ") }
        whitespace = true
        indention = true
    }

    mutating func increaseIndent(flow: Bool, indentless: Bool) {
        indents.append(indent)
        if indent < 0 {
            indent = flow ? 2 : 0
        } else if !indentless {
            indent += 2
        }
    }

    mutating func popIndent() { indent = indents.removeLast() }

    mutating func node(_ v: JSON, mappingContext: Bool, simpleKey: Bool) {
        switch v {
        case .object(let d):
            if d.isEmpty { flowEmpty("{", "}") } else { blockMapping(d) }
        case .array(let a):
            if a.isEmpty { flowEmpty("[", "]") } else { blockSequence(a, mappingContext) }
        case .null: plainScalar("null")
        case .bool(let b): plainScalar(b ? "true" : "false")
        case .integer(let i): plainScalar(String(i))
        case .double(let d): plainScalar(Self.float(d))
        case .string(let s): stringScalar(s, simpleKey: simpleKey)
        }
    }

    /// PyYAML `represent_float`.
    static func float(_ d: Double) -> String {
        if d.isNaN { return ".nan" }
        if d.isInfinite { return d < 0 ? "-.inf" : ".inf" }
        var v = pyFloatRepr(d).lowercased()
        if !v.contains("."), let e = v.firstIndex(of: "e") { v.replaceSubrange(e...e, with: ".0e") }
        return v
    }

    mutating func flowEmpty(_ open: String, _ close: String) {
        writeIndicator(open, true, true, false)
        increaseIndent(flow: true, indentless: false)
        popIndent()
        writeIndicator(close, false, false, false)
    }

    mutating func blockSequence(_ a: [JSON], _ mappingContext: Bool) {
        increaseIndent(flow: false, indentless: mappingContext && !indention)
        for item in a {
            writeIndent()
            writeIndicator("-", true, false, true)
            node(item, mappingContext: false, simpleKey: false)
        }
        popIndent()
    }

    mutating func blockMapping(_ d: OrderedDictionary<String, JSON>) {
        increaseIndent(flow: false, indentless: false)
        for (k, v) in d {
            writeIndent()
            let a = ScalarAnalysis(k)
            if !a.multiline && a.length <= 128 {
                stringScalar(k, simpleKey: true)
                writeIndicator(":", false, false, false)
            } else {
                writeIndicator("?", true, false, true)
                stringScalar(k, simpleKey: false)
                writeIndent()
                writeIndicator(":", true, false, true)
            }
            node(v, mappingContext: true, simpleKey: false)
        }
        popIndent()
    }

    mutating func plainScalar(_ s: String) {
        increaseIndent(flow: true, indentless: false)
        writePlain(s)
        popIndent()
    }

    mutating func stringScalar(_ s: String, simpleKey: Bool) {
        let u = Array(s.unicodeScalars)
        let a = ScalarAnalysis(s)
        var style: Style
        if u.contains("\n") {
            let n = u.count
            let keep = (n == 1) || (n >= 2 && u[n - 1] == "\n" && u[n - 2] == "\n")
            style = keep ? .double : .literal
        } else if Resolve11.core12Number.matches(s) {
            style = .single
        } else {
            style = .plain
        }
        if simpleKey && a.multiline { style = .double }
        if style == .plain {
            if !a.blockPlainAllowed { style = .single }
            if a.length == 0 && simpleKey { style = .single }
            if !Resolve11.isStr(s) { style = .single }
        }
        if style == .single && !a.singleQuotedAllowed { style = .double }
        if style == .literal && (!a.blockAllowed || simpleKey) { style = .double }
        increaseIndent(flow: true, indentless: false)
        switch style {
        case .plain: writePlain(s)
        case .single: writeSingle(u)
        case .double: writeDouble(u)
        case .literal: writeLiteral(u)
        }
        popIndent()
    }

    mutating func writePlain(_ s: String) {
        if !whitespace && !s.isEmpty { put(" ") }
        put(s)
        whitespace = false
        indention = false
    }

    mutating func writeSingle(_ u: [Unicode.Scalar]) {
        writeIndicator("'", true, false, false)
        var breaks = false
        for c in u {
            if c == " " {
                put(c)
            } else if isBreak(c) {
                if !breaks && c == "\n" { putBreak() }
                writeBreak(c)
                indention = true
                breaks = true
            } else {
                if breaks { writeIndent() }
                put(c)
                if c == "'" { put("'") }
                indention = false
                breaks = false
            }
        }
        if breaks { writeIndent() }
        writeIndicator("'", false, false, false)
    }

    mutating func writeDouble(_ u: [Unicode.Scalar]) {
        writeIndicator("\"", true, false, false)
        for c in u {
            if !isPrintable(c) || c.value == 0xFEFF || isBreak(c) || c == "\"" || c == "\\" {
                put("\\")
                switch c.value {
                case 0x00: put("0")
                case 0x07: put("a")
                case 0x08: put("b")
                case 0x09: put("t")
                case 0x0A: put("n")
                case 0x0B: put("v")
                case 0x0C: put("f")
                case 0x0D: put("r")
                case 0x1B: put("e")
                case 0x22: put("\"")
                case 0x5C: put("\\")
                case 0x85: put("N")
                case 0xA0: put("_")
                case 0x2028: put("L")
                case 0x2029: put("P")
                default:
                    let (tag, width) = c.value <= 0xFF ? ("x", 2) : c.value <= 0xFFFF ? ("u", 4) : ("U", 8)
                    let hex = String(c.value, radix: 16, uppercase: true)
                    put(tag + String(repeating: "0", count: width - hex.count) + hex)
                }
            } else {
                put(c)
            }
        }
        writeIndicator("\"", false, false, false)
    }

    mutating func writeLiteral(_ u: [Unicode.Scalar]) {
        writeIndicator("|", true, false, false)
        if let first = u.first, first == " " || isBreak(first) {
            writeIndicator("2", false, false, false)
        }
        // libyaml's block-scalar hints reset `open_ended`; only a `|+` sets it.
        openEnded = false
        var chomp: String? = nil
        if u.isEmpty || !isBreak(u[u.count - 1]) {
            chomp = "-"
        } else if u.count == 1 || isBreak(u[u.count - 2]) {
            chomp = "+"
            openEnded = true
        }
        if let chomp { writeIndicator(chomp, false, false, false) }
        putBreak()
        indention = true
        whitespace = true
        var breaks = true
        for c in u {
            if isBreak(c) {
                writeBreak(c)
                indention = true
                breaks = true
            } else {
                if breaks { writeIndent() }
                put(c)
                indention = false
                breaks = false
            }
        }
    }
}
