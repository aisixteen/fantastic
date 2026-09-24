// The message log's on-disk form: ONE XML document, rows appended, root never
// closed.
//
//     <messages v="1">
//     <row id="412" agent="ai" kind="body" parent="408" ts="1789000000.12">
//     <content>PRJ-1BCA4F
//
//     # Report — a line with &lt;/content&gt; inside is fine, the library escaped it
//     </content>
//     </row>
//     …
//
// A rule-for-rule port of python's
// `bundled_agents/loader/kernel_state/src/kernel_state/messages_xml.py`, which
// is canonical — and BYTE-FOR-BYTE with it, because every runtime appends to
// the same kind of file and a row written by one must be indistinguishable
// from a row written by another. Rust: `fantastic-kernel/src/messages_xml.rs`.
//
// Why XML and not JSON lines (user, 2026-09-09): the model↔harness path carries
// no JSON, and the file is a STREAM whose root is left open, read one completed
// `<row>` at a time; EOF without `</messages>` is the normal end. Foundation's
// `XMLParser` errors at EOF on the unclosed root, which is this log's NORMAL
// end — hence a hand-rolled reader. Written and read by this module, never by
// hand: `<` and `&` in a stored body are escaped on the way out and restored
// on the way in, so a document that contains `</content>` cannot break the log.
// Real newlines stay real.
//
// Mapping (generic, no per-kind schema): a scalar field is an ATTRIBUTE, typed
// on the way back by the ONE scalar rule the CLI and the call grammar share
// (`coerceScalar`); a text field (`content`, `title`, `why`, `text`, `summary`,
// or any string holding a newline) is a child ELEMENT; a list is a child
// element of `<item>`s; a nested object is a child element carrying its own
// attributes/children; a key that is NOT an XML name is
// `<entry key="…">value</entry>`. `null` is omitted (absent on read = null for
// every reader that asks). A truncated last row (a crash mid-append) is a PARSE
// ERROR naming the last good row — fail loud, no repair.
//
// RAW BYTES ARE A LENGTH. `<key len="N"/>`, read back as `{"len": N}` — the ONE
// reduction the tape makes (base64 is banned and a length is the honest record).
// Python holds those bytes INSIDE the payload/result dict and reduces them here;
// swift carries them on the binary side channel (`Item.binary`, `Outcome.body`),
// so the reduction happens where the ROW is built — `BatchSend.bytesLen` — and
// the element this writer emits for the resulting `{"len": N}` object is
// byte-for-byte python's.

import FantasticJSON
import Foundation
import OrderedCollections

public enum MessagesXML {
    /// The log format's version, stamped in the root and checked on read.
    public static let version = "1"
    /// The root, written ONCE at the head of a new log and NEVER closed — the
    /// file is a stream, and EOF is how it ends.
    public static let header = "<messages v=\"1\">\n"
    /// The field names that are always a child ELEMENT, whatever they hold.
    static let textKeys: Set<String> = ["content", "title", "why", "text", "summary"]

    // MARK: - write

    /// One row as one `<row>…</row>` element plus a newline.
    public static func rowToXml(_ row: Row) -> String {
        var attrs = ""
        var children = ""
        fill(row, &attrs, &children)
        // ElementTree writes `<row />` — with the space — for an element that
        // has no children and no text. Byte parity includes the space.
        return children.isEmpty ? "<row\(attrs) />\n" : "<row\(attrs)>\(children)</row>\n"
    }

    /// A MAP KEY IS NOT ALWAYS A TAG. Rows carry caller data now (a call's
    /// payload, a settle's result), and those maps are keyed by whatever the
    /// caller wrote — `callable_by: {"$parent": "*"}`, a grant map keyed by an
    /// agent id. A key that is not an XML name goes through
    /// `<entry key="…">value</entry>` instead of becoming an attribute or an
    /// element; everything else is unchanged. Python `_is_xml_name`.
    static func isXMLName(_ k: String) -> Bool {
        // python's `[A-Za-z_][A-Za-z0-9_.\-]*`, ASCII by construction.
        func alpha(_ c: Unicode.Scalar) -> Bool {
            (c >= "A" && c <= "Z") || (c >= "a" && c <= "z")
        }
        func digit(_ c: Unicode.Scalar) -> Bool { c >= "0" && c <= "9" }
        var first = true
        for c in k.unicodeScalars {
            let ok =
                first
                ? (alpha(c) || c == "_")
                : (alpha(c) || digit(c) || c == "_" || c == "." || c == "-")
            if !ok { return false }
            first = false
        }
        return !first
    }

    private static func fill(_ obj: Row, _ attrs: inout String, _ children: inout String) {
        for (k, v) in obj {
            if !isXMLName(k) {
                // `null` is omitted here too — python's `_fill` skips a None
                // BEFORE it ever asks whether the key is a tag.
                if v == .null { continue }
                children += entryElement(k, v)
                continue
            }
            switch v {
            // `null` IS OMITTED. Absent on read is null for every reader that
            // asks, so writing it would only make the file bigger.
            case .null: break
            case .bool(let b): attr(&attrs, k, b ? "true" : "false")
            case .integer(let n): attr(&attrs, k, String(n))
            case .double(let d): attr(&attrs, k, pyRepr(d))
            case .string(let s):
                // SCALARS, not graphemes: a "\r\n" is one Character to Swift and
                // would hide from `contains("\n")`.
                if textKeys.contains(k) || hasNewline(s) {
                    children += element(k, escapeText(s))
                } else {
                    attr(&attrs, k, s)
                }
            case .array(let items):
                children += element(k, items.map(itemElement).joined())
            case .object(let map):
                var a = ""
                var c = ""
                fill(map, &a, &c)
                children += openClose(k, a, c)
            }
        }
    }

    /// `<entry key="…">value</entry>` — the carrier for a map key that is not
    /// an XML name. The VALUE renders by python's `_fill_value` rules, and `key`
    /// is written FIRST so the attribute order matches ElementTree's.
    private static func entryElement(_ key: String, _ v: JSON) -> String {
        var attrs = " key=\"\(escapeAttr(key))\""
        var children = ""
        var text = ""
        switch v {
        case .null: attrs += " null=\"true\""
        case .bool(let b): text = b ? "true" : "false"
        case .integer(let n): text = String(n)
        case .double(let d): text = pyRepr(d)
        case .string(let s): text = escapeText(s)
        case .array(let items): children = items.map(itemElement).joined()
        case .object(let map):
            var a = ""
            var c = ""
            fill(map, &a, &c)
            attrs += a
            children = c
        }
        let inner = children.isEmpty ? text : children
        return inner.isEmpty ? "<entry\(attrs) />" : "<entry\(attrs)>\(inner)</entry>"
    }

    /// One `<item>` of a list.
    private static func itemElement(_ v: JSON) -> String {
        switch v {
        case .object(let map):
            var a = ""
            var c = ""
            fill(map, &a, &c)
            return openClose("item", a, c)
        case .array(let items): return element("item", items.map(itemElement).joined())
        // `null` inside a list is the ONE place null is representable: an
        // element that says so, because a missing `<item>` would change the
        // list's length.
        case .null: return "<item null=\"true\" />"
        case .bool(let b): return element("item", b ? "true" : "false")
        case .integer(let n): return element("item", String(n))
        case .double(let d): return element("item", pyRepr(d))
        case .string(let s): return element("item", escapeText(s))
        }
    }

    static func hasNewline(_ s: String) -> Bool {
        s.unicodeScalars.contains { $0 == "\n" || $0 == "\r" }
    }

    private static func element(_ name: String, _ inner: String) -> String {
        inner.isEmpty ? "<\(name) />" : "<\(name)>\(inner)</\(name)>"
    }

    private static func openClose(_ name: String, _ attrs: String, _ children: String) -> String {
        if attrs.isEmpty && children.isEmpty { return "<\(name) />" }
        if children.isEmpty { return "<\(name)\(attrs) />" }
        return "<\(name)\(attrs)>\(children)</\(name)>"
    }

    private static func attr(_ out: inout String, _ k: String, _ v: String) {
        out += " \(k)=\"\(escapeAttr(v))\""
    }

    /// A float the way python's `repr` writes it — `1e+16`, not
    /// `10000000000000000`, and `3.0`, not `3`. Swift's shortest round-trip
    /// description agrees with python's repr on the digits; the exponent
    /// switch (at 1e16 and below 1e-4) and its `e+` spelling are matched here.
    static func pyRepr(_ f: Double) -> String {
        if f.isNaN { return "nan" }
        if f.isInfinite { return f < 0 ? "-inf" : "inf" }
        if f == 0 { return f.sign == .minus ? "-0.0" : "0.0" }
        let plain = "\(f)"
        if abs(f) >= 1e16 || abs(f) < 1e-4 {
            // Swift already writes `1e+16` / `1e-05` for these magnitudes.
            if plain.contains("e") { return plain }
            let e = String(format: "%.17g", f)
            return e
        }
        if plain.contains(".") || plain.contains("e") { return plain }
        return plain + ".0"
    }

    /// ElementTree's ATTRIBUTE escaping: `&`, `<`, `>`, `"`, and the three
    /// whitespace characters that would not survive attribute-value
    /// normalization. `'` is NOT escaped. Note `&#09;`, zero-padded, which is
    /// ET's spelling.
    static func escapeAttr(_ s: String) -> String {
        var out = ""
        out.reserveCapacity(s.utf8.count)
        for c in s.unicodeScalars {
            switch c {
            case "&": out += "&amp;"
            case "<": out += "&lt;"
            case ">": out += "&gt;"
            case "\"": out += "&quot;"
            case "\n": out += "&#10;"
            case "\r": out += "&#13;"
            case "\t": out += "&#09;"
            default: out.unicodeScalars.append(c)
            }
        }
        return out
    }

    /// ElementTree's TEXT escaping: `&`, `<`, `>` only. Quotes, tabs and
    /// newlines go through verbatim — which is the whole point of storing a
    /// document here.
    static func escapeText(_ s: String) -> String {
        var out = ""
        out.reserveCapacity(s.utf8.count)
        for c in s.unicodeScalars {
            switch c {
            case "&": out += "&amp;"
            case "<": out += "&lt;"
            case ">": out += "&gt;"
            default: out.unicodeScalars.append(c)
            }
        }
        return out
    }

    // MARK: - read

    /// Every completed `<row>` in the text, in order.
    ///
    /// The document's root is open-ended: EOF without `</messages>` is normal.
    /// A row that OPENED and never closed is a CUT FILE, said loudly — the
    /// count of `<row` openers is exact because the writer escapes `<` inside
    /// every stored body, so the sequence can never appear in content.
    public static func parseMessages(_ text: String) throws -> [Row] {
        let bytes = Array(text.utf8)
        let opened = count(bytes, of: Array("<row ".utf8)) + count(bytes, of: Array("<row>".utf8))
        var rows: [Row] = []
        var i = 0
        let marker = Array("<row".utf8)
        while let at = find(bytes, marker, from: i) {
            // `<row` must be followed by a space, `>` or `/` — `<rowdy>` is not a row.
            let after = at + 4
            guard after < bytes.count, [UInt8(ascii: " "), UInt8(ascii: ">"), UInt8(ascii: "/")].contains(bytes[after])
            else {
                i = after
                continue
            }
            guard let (elem, end) = parseElement(bytes, at) else { break }
            rows.append(elem.fields)
            i = end
        }
        if opened != rows.count {
            let last = rows.last?["id"].map { $0.asInt.map(String.init) ?? $0.serialize() } ?? "none"
            throw MessagesXMLError.cut(
                "messages log: \(opened - rows.count) row(s) opened and never closed — the last append "
                    + "was cut (a crash mid-write). Truncate the file at the last complete "
                    + "</row> (row \(last)) — nothing is repaired automatically")
        }
        return rows
    }

    private struct Element {
        var name: String
        var fields: Row
        /// A run of `<item>` children, kept apart from the fields.
        var items: [JSON]? = nil
        /// A leaf's text.
        var text: String? = nil
        /// `<entry>`'s `key`, RAW — a map key is a string, never coerced. `7`
        /// as a key is the string "7", and python's `child.get("key")` says so.
        var rawKey: String? = nil
    }

    /// One element starting at `at`: `(element, index just past its close)`.
    private static func parseElement(_ b: [UInt8], _ at: Int) -> (Element, Int)? {
        guard let headEnd = find(b, [UInt8(ascii: ">")], from: at + 1) else { return nil }
        var head = String(decoding: b[(at + 1)..<headEnd], as: UTF8.self)
        let selfClosing = head.hasSuffix("/")
        if selfClosing { head.removeLast() }
        let nameEnd = head.firstIndex(where: { $0.isWhitespace }) ?? head.endIndex
        let name = String(head[..<nameEnd])
        var elem = Element(name: name, fields: [:])
        for (k, v) in parseAttrs(String(head[nameEnd...])) {
            if name == "entry" && k == "key" {
                elem.rawKey = v
                continue
            }
            elem.fields[k] = coerceScalar(v)
        }
        if selfClosing { return (elem, headEnd + 1) }
        let closer = Array("</\(name)>".utf8)
        var i = headEnd + 1
        var textRun: [UInt8] = []
        while true {
            guard let tag = find(b, [UInt8(ascii: "<")], from: i) else { return nil }
            textRun.append(contentsOf: b[i..<tag])
            if startsWith(b, closer, at: tag) {
                let body = unescape(String(decoding: textRun, as: UTF8.self))
                // A LEAF holds text; a BRANCH holds children. The writer never
                // mixes them, so whitespace between child elements is layout
                // and anything else is a leaf's value.
                let isBranch = !elem.fields.isEmpty || elem.items != nil
                let keep = isBranch ? !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty : !body.isEmpty
                if keep { elem.text = body }
                return (elem, tag + closer.count)
            }
            guard let (child, end) = parseElement(b, tag) else { return nil }
            // `<entry key="…">` lands under the KEY IT NAMES, and the value is
            // read by the ordinary rules with `key` stripped off first (python
            // `_without_key`). Keys stay strings — a numeric-looking one is not
            // coerced, because it was a map key, not a scalar field.
            if child.name == "entry", let key = child.rawKey {
                elem.fields[key] = valueOf(child)
                textRun.removeAll(keepingCapacity: true)
                i = end
                continue
            }
            let value = valueOf(child)
            if child.name == "item" {
                elem.items = (elem.items ?? []) + [value]
            } else {
                elem.fields[child.name] = value
            }
            textRun.removeAll(keepingCapacity: true)
            i = end
        }
    }

    /// A parsed element as the value its parent should hold — by the same
    /// rules the writer used: a run of `<item>`s is a list, `null="true"` is
    /// null, a leaf's text is typed by the one scalar rule unless the field is
    /// a text field.
    private static func valueOf(_ child: Element) -> JSON {
        if child.fields["null"] == .string("true") || child.fields["null"] == .bool(true) {
            return .null
        }
        if let items = child.items { return .array(items) }
        if let t = child.text {
            if child.fields.isEmpty {
                return (textKeys.contains(child.name) || t.unicodeScalars.contains("\n")) ? .string(t) : coerceScalar(t)
            }
            var f = child.fields
            f["#text"] = .string(t)
            return .object(f)
        }
        // An empty element: `""` for a text field, and the one scalar rule's
        // answer for anything else (also `""`).
        return child.fields.isEmpty ? .string("") : .object(child.fields)
    }

    private static func parseAttrs(_ head: String) -> [(String, String)] {
        var out: [(String, String)] = []
        let b = Array(head)
        var i = 0
        while i < b.count {
            while i < b.count && b[i].isWhitespace { i += 1 }
            let start = i
            while i < b.count && b[i] != "=" && !b[i].isWhitespace { i += 1 }
            if start == i || i >= b.count { break }
            let key = String(b[start..<i])
            while i < b.count && (b[i] == "=" || b[i].isWhitespace) { i += 1 }
            if i >= b.count || b[i] != "\"" { break }
            i += 1
            let vstart = i
            while i < b.count && b[i] != "\"" { i += 1 }
            let raw = String(b[vstart..<i])
            i += 1
            out.append((key, unescape(raw)))
        }
        return out
    }

    /// The entities the writer emits, after the newline normalization every
    /// conforming XML reader applies (XML 1.0 §2.11).
    ///
    /// ORDER IS LOAD-BEARING: normalization runs on the RAW text and unescaping
    /// after it, so a LITERAL `\r\n` in element text becomes `\n` (lossy, and
    /// python is lossy the same way — ElementTree writes that `\r` unescaped)
    /// while an explicitly escaped `&#13;` in an attribute survives as a
    /// carriage return. Verified against `xml.etree.ElementTree` 2026-09-10.
    static func unescape(_ raw: String) -> String {
        let s = Array(normalizeNewlines(raw).unicodeScalars)
        var out = String.UnicodeScalarView()
        var i = 0
        while i < s.count {
            let c = s[i]
            if c == "&" {
                let rest = String(String.UnicodeScalarView(s[(i + 1)..<min(i + 7, s.count)]))
                let table: [(String, Unicode.Scalar)] = [
                    ("amp;", "&"), ("lt;", "<"), ("gt;", ">"), ("quot;", "\""), ("apos;", "'"),
                    ("#10;", "\n"), ("#13;", "\r"), ("#09;", "\t"), ("#9;", "\t"),
                ]
                if let (ent, ch) = table.first(where: { rest.hasPrefix($0.0) }) {
                    out.append(ch)
                    i += 1 + ent.count
                    continue
                }
            }
            out.append(c)
            i += 1
        }
        return String(out)
    }

    private static func normalizeNewlines(_ s: String) -> String {
        guard s.unicodeScalars.contains("\r") else { return s }
        var out = String.UnicodeScalarView()
        var it = s.unicodeScalars.makeIterator()
        var pending: Unicode.Scalar? = it.next()
        while let c = pending {
            let next = it.next()
            if c == "\r" {
                out.append("\n")
                pending = (next == "\n") ? it.next() : next
            } else {
                out.append(c)
                pending = next
            }
        }
        return String(out)
    }

    // MARK: - byte helpers

    private static func find(_ b: [UInt8], _ pat: [UInt8], from: Int) -> Int? {
        guard !pat.isEmpty, b.count >= pat.count else { return nil }
        var i = max(from, 0)
        let last = b.count - pat.count
        let first = pat[0]
        while i <= last {
            if b[i] == first && startsWith(b, pat, at: i) { return i }
            i += 1
        }
        return nil
    }

    private static func startsWith(_ b: [UInt8], _ pat: [UInt8], at: Int) -> Bool {
        guard at + pat.count <= b.count else { return false }
        for j in 0..<pat.count where b[at + j] != pat[j] { return false }
        return true
    }

    private static func count(_ b: [UInt8], of pat: [UInt8]) -> Int {
        var n = 0
        var i = 0
        while let at = find(b, pat, from: i) {
            n += 1
            i = at + pat.count
        }
        return n
    }
}

/// The one way the reader refuses: a cut last row.
public enum MessagesXMLError: Error, CustomStringConvertible {
    case cut(String)
    public var description: String {
        switch self {
        case .cut(let s): return s
        }
    }
}
