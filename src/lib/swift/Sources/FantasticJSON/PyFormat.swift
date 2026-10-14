// Python-format helpers — the texts python writes, reproduced exactly.
//
// Python is the canonical kernel, and its refusal and reply texts are built with
// `{x!r}`, `str(x)`, `json.dumps(x)`, `str(OSError)` and `int(x)`. A port that
// approximates any of them diverges byte-for-byte on every text that embeds one.
// These live here, in FantasticJSON, because every target already depends on it:
// one copy serves the bundles, the AI core and the CLI.
//
// Each helper names the CPython routine it mirrors; the pins in
// `Tests/FantasticJSONTests/PyFormatTests.swift` were measured with CPython 3.11.

import Foundation

// MARK: - repr / str

/// Python's `repr(str)` (`{x!r}`, `%r`): CPython `unicode_repr`.
///
/// Single quotes, unless the text holds a `'` and no `"`. The chosen quote and
/// `\` are escaped; TAB/LF/CR as `\t` `\n` `\r`; other C0 controls and U+007F as
/// `\xHH`; a non-ASCII scalar is written raw when python calls it printable,
/// otherwise as `\xHH`, `\uHHHH` or `\UHHHHHHHH`.
public func pyRepr(_ s: String) -> String {
    let quote: Unicode.Scalar = (s.contains("'") && !s.contains("\"")) ? "\"" : "'"
    var out = String.UnicodeScalarView()
    out.append(quote)
    for c in s.unicodeScalars {
        switch c {
        case quote, "\\":
            out.append("\\")
            out.append(c)
        case "\t": out.append(contentsOf: "\\t".unicodeScalars)
        case "\n": out.append(contentsOf: "\\n".unicodeScalars)
        case "\r": out.append(contentsOf: "\\r".unicodeScalars)
        default:
            if c.value < 0x20 || c.value == 0x7F {
                out.append(contentsOf: hexEscape("x", c.value, width: 2).unicodeScalars)
            } else if c.value < 0x7F || pyIsPrintable(c) {
                out.append(c)
            } else if c.value <= 0xFF {
                out.append(contentsOf: hexEscape("x", c.value, width: 2).unicodeScalars)
            } else if c.value <= 0xFFFF {
                out.append(contentsOf: hexEscape("u", c.value, width: 4).unicodeScalars)
            } else {
                out.append(contentsOf: hexEscape("U", c.value, width: 8).unicodeScalars)
            }
        }
    }
    out.append(quote)
    return String(out)
}

/// Python's `repr(x)` for a JSON value: a str is quoted, everything else is
/// spelled as `str(x)` (for None, bools, numbers, lists and dicts python's
/// `repr` and `str` agree).
public func pyRepr(_ v: JSON) -> String {
    if case .string(let s) = v { return pyRepr(s) }
    return pyStr(v)
}

/// Python's `str(x)` for a JSON value: a str as itself, `None`, `True`/`False`,
/// int digits, the float repr, and list/dict as python's container repr
/// (`', '` and `': '` separators, strings single-quoted inside).
public func pyStr(_ v: JSON) -> String {
    switch v {
    case .null: return "None"
    case .bool(let b): return b ? "True" : "False"
    case .integer(let n): return String(n)
    case .double(let d): return pyFloatRepr(d)
    case .string(let s): return s
    case .array(let a): return "[" + a.map(pyRepr).joined(separator: ", ") + "]"
    case .object(let o):
        return "{" + o.map { "\(pyRepr($0.key)): \(pyRepr($0.value))" }.joined(separator: ", ")
            + "}"
    }
}

/// Python's `repr(float)`: the shortest round-trip digits (Swift's description
/// produces the same digits), laid out by CPython's `format_float_short` rule —
/// exponent form when the decimal point sits at or below 10^-4 or above 10^16,
/// the exponent signed and at least two digits, and `.0` on a whole number.
public func pyFloatRepr(_ d: Double) -> String {
    if d.isNaN { return "nan" }
    if d.isInfinite { return d < 0 ? "-inf" : "inf" }
    let sign = d.sign == .minus ? "-" : ""
    if d == 0 { return sign + "0.0" }
    let (digits, decpt) = shortestDigits(abs(d))
    if decpt <= -4 || decpt > 16 {
        var mantissa = String(digits.first!)
        if digits.count > 1 { mantissa += "." + digits.dropFirst() }
        let exp = decpt - 1
        let expDigits = String(abs(exp))
        let padded = expDigits.count < 2 ? "0" + expDigits : expDigits
        return sign + mantissa + "e" + (exp < 0 ? "-" : "+") + padded
    }
    if decpt <= 0 {
        return sign + "0." + String(repeating: "0", count: -decpt) + digits
    }
    if decpt >= digits.count {
        return sign + digits + String(repeating: "0", count: decpt - digits.count) + ".0"
    }
    let cut = digits.index(digits.startIndex, offsetBy: decpt)
    return sign + digits[..<cut] + "." + digits[cut...]
}

/// The shortest round-trip significant digits of a finite positive double and
/// the decimal-point position (`value = 0.DIGITS × 10^decpt`), read off Swift's
/// shortest-representation description.
private func shortestDigits(_ d: Double) -> (String, Int) {
    let text = "\(d)"
    var mantissa = Substring(text)
    var exp = 0
    if let e = text.firstIndex(where: { $0 == "e" || $0 == "E" }) {
        mantissa = text[..<e]
        exp = Int(text[text.index(after: e)...]) ?? 0
    }
    let parts = mantissa.split(separator: ".", omittingEmptySubsequences: false)
    let intPart = parts.first.map(String.init) ?? ""
    let fracPart = parts.count > 1 ? String(parts[1]) : ""
    var digits = intPart + fracPart
    var decpt = intPart.count + exp
    while digits.first == "0" {
        digits.removeFirst()
        decpt -= 1
    }
    while digits.last == "0" { digits.removeLast() }
    return (digits, decpt)
}

// MARK: - truthiness

/// Python's truthiness of a JSON value: None, False, 0, 0.0, "", [] and {} are
/// false; everything else (NaN included) is true.
public func pyTruthy(_ v: JSON) -> Bool {
    switch v {
    case .null: return false
    case .bool(let b): return b
    case .integer(let i): return i != 0
    case .double(let d): return d != 0
    case .string(let s): return !s.isEmpty
    case .array(let a): return !a.isEmpty
    case .object(let o): return !o.isEmpty
    }
}

// MARK: - json.dumps

/// Python's `json.dumps(x, ensure_ascii=…, indent=…)` with every other argument
/// at its default.
///
/// Separators are CPython's: `', '` and `': '` compact, `','` + newline and
/// `': '` indented; empty containers stay `[]`/`{}`; keys keep insertion order;
/// floats use the float repr and non-finite ones `NaN`/`Infinity`/`-Infinity`
/// (`allow_nan=True`). `ensure_ascii` escapes everything outside `' '..'~'` as
/// `\uXXXX`, astral scalars as a surrogate pair; without it only `"`, `\` and
/// C0 controls are escaped.
public func pyJsonDumps(_ v: JSON, ensureAscii: Bool = true, indent: Int? = nil) -> String {
    var out = ""
    pyDumpsValue(v, ensureAscii: ensureAscii, indent: indent, level: 0, into: &out)
    return out
}

private func pyDumpsValue(
    _ v: JSON, ensureAscii: Bool, indent: Int?, level: Int, into out: inout String
) {
    switch v {
    case .null: out += "null"
    case .bool(let b): out += b ? "true" : "false"
    case .integer(let n): out += String(n)
    case .double(let d):
        if d.isNaN {
            out += "NaN"
        } else if d.isInfinite {
            out += d < 0 ? "-Infinity" : "Infinity"
        } else {
            out += pyFloatRepr(d)
        }
    case .string(let s): out += pyJsonString(s, ensureAscii: ensureAscii)
    case .array(let a):
        if a.isEmpty {
            out += "[]"
            return
        }
        out += "["
        for (i, item) in a.enumerated() {
            out += pyDumpsSeparator(first: i == 0, indent: indent, level: level + 1)
            pyDumpsValue(
                item, ensureAscii: ensureAscii, indent: indent, level: level + 1, into: &out)
        }
        out += pyDumpsClose(indent: indent, level: level) + "]"
    case .object(let o):
        if o.isEmpty {
            out += "{}"
            return
        }
        out += "{"
        for (i, (k, val)) in o.enumerated() {
            out += pyDumpsSeparator(first: i == 0, indent: indent, level: level + 1)
            out += pyJsonString(k, ensureAscii: ensureAscii) + ": "
            pyDumpsValue(
                val, ensureAscii: ensureAscii, indent: indent, level: level + 1, into: &out)
        }
        out += pyDumpsClose(indent: indent, level: level) + "}"
    }
}

private func pyDumpsSeparator(first: Bool, indent: Int?, level: Int) -> String {
    guard let indent else { return first ? "" : ", " }
    return (first ? "" : ",") + "\n" + String(repeating: " ", count: indent * level)
}

private func pyDumpsClose(indent: Int?, level: Int) -> String {
    guard let indent else { return "" }
    return "\n" + String(repeating: " ", count: indent * level)
}

/// CPython `py_encode_basestring_ascii` / `py_encode_basestring`.
private func pyJsonString(_ s: String, ensureAscii: Bool) -> String {
    var out = "\""
    for c in s.unicodeScalars {
        switch c {
        case "\"": out += "\\\""
        case "\\": out += "\\\\"
        case "\n": out += "\\n"
        case "\r": out += "\\r"
        case "\t": out += "\\t"
        case "\u{08}": out += "\\b"
        case "\u{0C}": out += "\\f"
        default:
            if c.value < 0x20 {
                out += hexEscape("u", c.value, width: 4)
            } else if !ensureAscii || c.value <= 0x7E {
                out.unicodeScalars.append(c)
            } else if c.value <= 0xFFFF {
                out += hexEscape("u", c.value, width: 4)
            } else {
                let v = c.value - 0x10000
                out += hexEscape("u", 0xD800 | (v >> 10), width: 4)
                out += hexEscape("u", 0xDC00 | (v & 0x3FF), width: 4)
            }
        }
    }
    return out + "\""
}

// MARK: - OSError

/// Python's `str(OSError)`: `[Errno N] <strerror>`, then `: <repr(path)>` when
/// the error names a file, and ` -> <repr(path2)>` when it names two (a rename).
public func osErrorStr(_ code: Int32, _ path: String? = nil, _ path2: String? = nil) -> String {
    var out = "[Errno \(code)] \(String(cString: strerror(code)))"
    if let path {
        out += ": " + pyRepr(path)
        if let path2 { out += " -> " + pyRepr(path2) }
    }
    return out
}

/// The `OSError` SUBCLASS python raises for an errno (CPython `errnomap`) —
/// what `type(e).__name__` names when the kernel stamps a raised handler.
public func pyOSErrorName(_ code: Int32) -> String {
    switch code {
    case EAGAIN, EALREADY, EINPROGRESS: return "BlockingIOError"
    case EPIPE, ESHUTDOWN: return "BrokenPipeError"
    case ECHILD: return "ChildProcessError"
    case ECONNABORTED: return "ConnectionAbortedError"
    case ECONNREFUSED: return "ConnectionRefusedError"
    case ECONNRESET: return "ConnectionResetError"
    case EEXIST: return "FileExistsError"
    case ENOENT: return "FileNotFoundError"
    case EISDIR: return "IsADirectoryError"
    case ENOTDIR: return "NotADirectoryError"
    case EINTR: return "InterruptedError"
    case EACCES, EPERM: return "PermissionError"
    case ESRCH: return "ProcessLookupError"
    case ETIMEDOUT: return "TimeoutError"
    default: return "OSError"
    }
}

/// The python exception an `OSError` raise lands as: the errno's subclass and
/// `str(OSError)`, so `description` is the `"<Class>: [Errno N] …"` the kernel
/// writes for a handler that raised.
public func pyOSError(_ code: Int32, _ path: String? = nil) -> PyError {
    PyError(pyOSErrorName(code), osErrorStr(code, path))
}

// MARK: - UnicodeDecodeError

/// `str(UnicodeDecodeError)` for the FIRST invalid sequence in `data`, or nil
/// when it decodes — CPython's utf-8 decoder, error position and wording
/// included (`'utf-8' codec can't decode byte 0xff in position 0: invalid start
/// byte`). A `read_text` that cannot decode prints this verbatim, and it is a
/// `ValueError`, so python catches it beside its `OSError`s.
public func pyUtf8DecodeError(_ data: Data) -> String? {
    let b = [UInt8](data)
    let n = b.count
    func isCont(_ x: UInt8) -> Bool { x & 0xC0 == 0x80 }
    func message(_ start: Int, _ end: Int, _ why: String) -> String {
        if end - start == 1 {
            return
                "'utf-8' codec can't decode byte 0x\(hex2(b[start])) in position \(start): \(why)"
        }
        return "'utf-8' codec can't decode bytes in position \(start)-\(end - 1): \(why)"
    }
    var i = 0
    while i < n {
        let lead = b[i]
        if lead < 0x80 {
            i += 1
            continue
        }
        let extra: Int
        switch lead {
        case 0xC2...0xDF: extra = 1
        case 0xE0...0xEF: extra = 2
        case 0xF0...0xF4: extra = 3
        default: return message(i, i + 1, "invalid start byte")
        }
        for k in 1...extra {
            guard i + k < n else { return message(i, n, "unexpected end of data") }
            let c = b[i + k]
            let ok: Bool
            if k == 1 {
                switch lead {
                case 0xE0: ok = (0xA0...0xBF).contains(c)
                case 0xED: ok = (0x80...0x9F).contains(c)
                case 0xF0: ok = (0x90...0xBF).contains(c)
                case 0xF4: ok = (0x80...0x8F).contains(c)
                default: ok = isCont(c)
                }
            } else {
                ok = isCont(c)
            }
            if !ok { return message(i, i + k, "invalid continuation byte") }
        }
        i += extra + 1
    }
    return nil
}

private func hex2(_ v: UInt8) -> String {
    let h = String(v, radix: 16)
    return h.count < 2 ? "0" + h : h
}

// MARK: - type names

/// Python's `type(x).__name__` for a JSON value — the word a `TypeError` or an
/// `AttributeError` text names (`'int' object has no attribute 'encode'`).
public func pyTypeName(_ v: JSON) -> String {
    switch v {
    case .null: return "NoneType"
    case .bool: return "bool"
    case .integer: return "int"
    case .double: return "float"
    case .string: return "str"
    case .array: return "list"
    case .object: return "dict"
    }
}

// MARK: - int()

/// A python exception a ported expression raises: `type` is the class name
/// (`ValueError`), `message` its `str`. `description` is python's
/// `"<type>: <message>"` rendering.
public struct PyError: Error, Equatable, CustomStringConvertible, Sendable {
    public let type: String
    public let message: String

    public init(_ type: String, _ message: String) {
        self.type = type
        self.message = message
    }

    public var description: String { "\(type): \(message)" }
}

/// `str(e)` — the message alone, WITHOUT the `<Type>: ` prefix.
///
/// Python surfaces a raised exception TWICE and differently: the caller's
/// settle carries `f"{type(e).__name__}: {e}"` (the kernel builds that), while
/// a handler reporting its own failure carries the bare `str(e)`. Rendering the
/// prefixed form in both places put `RuntimeError:` inside every status event.
public func pyErrorText(_ error: Error) -> String {
    if let e = error as? PyError { return e.message }
    return "\(error)"
}

/// Python's `int(x)` for a JSON value.
///
/// A bool is 0/1, a float truncates, a str parses python's way (python-isspace
/// strip, optional sign, Unicode decimal digits, single `_` between digits);
/// None, a list or a dict is python's `TypeError`, an unparseable str its
/// `ValueError`. A value wider than `Int` is `OverflowError` — python holds it,
/// this cannot.
public func pyInt(_ v: JSON) throws(PyError) -> Int {
    switch v {
    case .bool(let b): return b ? 1 : 0
    case .integer(let n):
        guard let i = Int(exactly: n) else { throw overflow() }
        return i
    case .double(let d):
        if d.isNaN { throw PyError("ValueError", "cannot convert float NaN to integer") }
        if d.isInfinite {
            throw PyError("OverflowError", "cannot convert float infinity to integer")
        }
        guard let i = Int(exactly: d.rounded(.towardZero)) else { throw overflow() }
        return i
    case .string(let s): return try pyIntFromString(s)
    case .null: throw intTypeError("NoneType")
    case .array: throw intTypeError("list")
    case .object: throw intTypeError("dict")
    }
}

/// Python's `int(x or default)`: a falsy value is the default, anything else is
/// `int(x)` (and raises as `int(x)` does).
public func pyIntOr(_ v: JSON, _ dflt: Int) throws(PyError) -> Int {
    guard pyTruthy(v) else { return dflt }
    return try pyInt(v)
}

private func intTypeError(_ typeName: String) -> PyError {
    PyError(
        "TypeError",
        "int() argument must be a string, a bytes-like object or a real number, not '\(typeName)'")
}

private func overflow() -> PyError {
    PyError("OverflowError", "Python int too large to convert to C long")
}

/// CPython `PyLong_FromUnicodeObject(s, 10)`.
private func pyIntFromString(_ s: String) throws(PyError) -> Int {
    let invalid = PyError(
        "ValueError",
        "invalid literal for int() with base 10: "
            + String(String.UnicodeScalarView(pyRepr(s).unicodeScalars.prefix(200))))
    // `_PyUnicode_TransformDecimalAndSpaceToASCII`: whitespace → ' ', a decimal
    // digit of any script → its ASCII digit, any other non-ASCII scalar → invalid.
    var chars: [UInt8] = []
    for c in s.unicodeScalars {
        if pyIsSpace(c) {
            chars.append(0x20)
        } else if c.properties.numericType == .decimal, let n = c.properties.numericValue {
            chars.append(0x30 + UInt8(n))
        } else if c.isASCII {
            chars.append(UInt8(c.value))
        } else {
            throw invalid
        }
    }
    var i = 0
    while i < chars.count, chars[i] == 0x20 { i += 1 }
    var end = chars.count
    while end > i, chars[end - 1] == 0x20 { end -= 1 }
    var negative = false
    if i < end, chars[i] == 0x2B || chars[i] == 0x2D {
        negative = chars[i] == 0x2D
        i += 1
    }
    guard i < end else { throw invalid }
    var value = 0
    var previousWasDigit = false
    while i < end {
        let ch = chars[i]
        if ch == 0x5F {
            // `_` only BETWEEN digits: not first, not doubled, not last.
            guard previousWasDigit, i + 1 < end, chars[i + 1] != 0x5F else { throw invalid }
            previousWasDigit = false
        } else if ch >= 0x30 && ch <= 0x39 {
            let (m, o1) = value.multipliedReportingOverflow(by: 10)
            let digit = Int(ch - 0x30)
            let (a, o2) =
                negative ? m.subtractingReportingOverflow(digit) : m.addingReportingOverflow(digit)
            if o1 || o2 { throw overflow() }
            value = a
            previousWasDigit = true
        } else {
            throw invalid
        }
        i += 1
    }
    return value
}

// MARK: - float()

/// Python's `float(x)` for a JSON value.
///
/// A bool is 1.0/0.0, an int widens, a str parses python's way (python-isspace
/// strip, optional sign, `inf`/`infinity`/`nan` in any case, Unicode decimal
/// digits, single `_` between digits); None, a list or a dict is python's
/// `TypeError`, an unparseable str its `ValueError`.
public func pyFloat(_ v: JSON) throws(PyError) -> Double {
    switch v {
    case .bool(let b): return b ? 1 : 0
    case .integer(let n): return Double(n)
    case .double(let d): return d
    case .string(let s): return try pyFloatFromString(s)
    case .null, .array, .object:
        throw PyError(
            "TypeError",
            "float() argument must be a string or a real number, not '\(pyTypeName(v))'")
    }
}

/// Python's `float(x or default)`: a falsy value is the default, anything else
/// is `float(x)` (and raises as `float(x)` does).
public func pyFloatOr(_ v: JSON, _ dflt: Double) throws(PyError) -> Double {
    guard pyTruthy(v) else { return dflt }
    return try pyFloat(v)
}

/// CPython `PyFloat_FromString` → `_Py_string_to_double_with_underscores`.
private func pyFloatFromString(_ s: String) throws(PyError) -> Double {
    let invalid = PyError("ValueError", "could not convert string to float: " + pyRepr(s))
    // `_PyUnicode_TransformDecimalAndSpaceToASCII`, as `int()` does it.
    var chars: [UInt8] = []
    for c in s.unicodeScalars {
        if pyIsSpace(c) {
            chars.append(0x20)
        } else if c.properties.numericType == .decimal, let n = c.properties.numericValue {
            chars.append(0x30 + UInt8(n))
        } else if c.isASCII {
            chars.append(UInt8(c.value))
        } else {
            throw invalid
        }
    }
    var i = 0
    var end = chars.count
    while i < end, chars[i] == 0x20 { i += 1 }
    while end > i, chars[end - 1] == 0x20 { end -= 1 }
    var body = Array(chars[i..<end])
    // `_` only BETWEEN digits, then dropped.
    if body.contains(0x5F) {
        for (k, ch) in body.enumerated() where ch == 0x5F {
            let prev = k > 0 ? body[k - 1] : 0
            let next = k + 1 < body.count ? body[k + 1] : 0
            guard isDigit(prev), isDigit(next) else { throw invalid }
        }
        body = body.filter { $0 != 0x5F }
    }
    guard !body.isEmpty else { throw invalid }
    var p = 0
    var sign = 1.0
    if body[p] == 0x2B || body[p] == 0x2D {
        if body[p] == 0x2D { sign = -1 }
        p += 1
    }
    let rest = String(decoding: body[p...], as: UTF8.self).lowercased()
    if rest == "inf" || rest == "infinity" { return sign * .infinity }
    if rest == "nan" { return sign * .nan }
    guard isDecimalLiteral(rest) else { throw invalid }
    return rest.withCString { cstr in
        var endp: UnsafeMutablePointer<CChar>?
        let d = strtod(cstr, &endp)
        return sign * d
    }
}

/// `digits[.digits?] | .digits`, then an optional `e[+-]digits` — CPython's
/// `_Py_dg_strtod` grammar (no hex, no leading `+`/`-`: the sign is peeled off).
private func isDecimalLiteral(_ s: String) -> Bool {
    let b = Array(s.utf8)
    var i = 0
    var intDigits = 0
    while i < b.count, isDigit(b[i]) {
        i += 1
        intDigits += 1
    }
    var fracDigits = 0
    if i < b.count, b[i] == 0x2E {
        i += 1
        while i < b.count, isDigit(b[i]) {
            i += 1
            fracDigits += 1
        }
    }
    guard intDigits + fracDigits > 0 else { return false }
    if i < b.count, b[i] == 0x65 {
        i += 1
        if i < b.count, b[i] == 0x2B || b[i] == 0x2D { i += 1 }
        var expDigits = 0
        while i < b.count, isDigit(b[i]) {
            i += 1
            expDigits += 1
        }
        guard expDigits > 0 else { return false }
    }
    return i == b.count
}

private func isDigit(_ c: UInt8) -> Bool { c >= 0x30 && c <= 0x39 }

// MARK: - character classes

/// Python's `str.isspace()` for one scalar (`Py_UNICODE_ISSPACE`): Unicode
/// category Zs, or bidirectional class WS/B/S — which adds U+001C..U+001F to
/// Unicode's White_Space set and drops U+180E.
public func pyIsSpace(_ c: Unicode.Scalar) -> Bool {
    switch c.value {
    case 0x09...0x0D, 0x1C...0x20, 0x85, 0xA0, 0x1680, 0x2000...0x200A, 0x2028, 0x2029, 0x202F,
        0x205F, 0x3000:
        return true
    default:
        return false
    }
}

/// Python's `str.isalnum()` for one scalar: `isalpha` (categories Lu, Ll, Lt,
/// Lm, Lo) OR any of `isdecimal`/`isdigit`/`isnumeric`, which together are
/// exactly "this scalar has a numeric value" — Nd, Nl, No, and the CJK and
/// superscript digits with them. `'²'`, `'一'` and `'٣'` are all alphanumeric to
/// python, and a swift `Character.isLetter || .isNumber` check on a COMBINED
/// grapheme disagreed with it in the other direction (`e` + U+0301 is one
/// Character here and two code points there).
public func pyIsAlnum(_ c: Unicode.Scalar) -> Bool {
    switch c.properties.generalCategory {
    case .uppercaseLetter, .lowercaseLetter, .titlecaseLetter, .modifierLetter, .otherLetter:
        return true
    default:
        return c.properties.numericType != nil
    }
}

/// Python's `str.isdigit()` for one scalar (`Py_UNICODE_ISDIGIT`): every scalar
/// CPython can read as a digit — the decimal digits of any script AND the
/// digit-valued others (the superscripts). NOT the same set `int()` takes:
/// `'٢'` is decimal and parses to 2, `'²'` is a digit and only a digit, so
/// `'²'.isdigit()` is true while `int('²')` raises. Everything reading an id
/// out of a model's text depends on that difference.
public func pyIsDigit(_ c: Unicode.Scalar) -> Bool {
    switch c.properties.numericType {
    case .decimal, .digit: return true
    default: return false
    }
}

/// Python's `str.isdigit()` for a whole string: non-empty and every scalar a
/// digit.
public func pyIsDigitString(_ s: String) -> Bool {
    !s.isEmpty && s.unicodeScalars.allSatisfy(pyIsDigit)
}

/// Python's `str.isdecimal()` for a whole string: non-empty and every scalar
/// `Numeric_Type=Decimal` — exactly the set `int()` accepts.
public func pyIsDecimalString(_ s: String) -> Bool {
    !s.isEmpty && s.unicodeScalars.allSatisfy { $0.properties.numericType == .decimal }
}

/// Python's `len(x)` for a JSON value: a list's items, a dict's keys, a str's
/// CODE POINTS. A number, a bool or `None` HAS no length — python raises
/// `TypeError` there, and a caller that swallowed it would answer a digest
/// python never produces.
public func pyLen(_ v: JSON) throws(PyError) -> Int {
    switch v {
    case .array(let a): return a.count
    case .object(let o): return o.count
    case .string(let s): return s.unicodeScalars.count
    default:
        throw PyError("TypeError", "object of type '\(pyTypeName(v))' has no len()")
    }
}

/// Python's `str.isprintable()` for one scalar (`Py_UNICODE_ISPRINTABLE`):
/// false for categories Cc, Cf, Cs, Co, Cn, Zl, Zp and Zs, except the ASCII space.
public func pyIsPrintable(_ c: Unicode.Scalar) -> Bool {
    if c == " " { return true }
    switch c.properties.generalCategory {
    case .control, .format, .surrogate, .privateUse, .unassigned, .lineSeparator,
        .paragraphSeparator, .spaceSeparator:
        return false
    default:
        return true
    }
}

private func hexEscape(_ tag: String, _ value: UInt32, width: Int) -> String {
    let hex = String(value, radix: 16)
    return "\\" + tag + String(repeating: "0", count: max(0, width - hex.count)) + hex
}

// MARK: - python text-mode universal newlines

/// Python's default text-mode newline translation (`open(..., newline=None)`,
/// which `Path.read_text()` uses): `\r\n` → `\n`, then a lone `\r` → `\n`.
///
/// Scalar-by-scalar, NOT by `Character`: swift reads a CRLF pair as ONE
/// grapheme, so a `Character`-based scan cannot see the boundary python's text
/// mode does (same reasoning as [`pySplitlines`]).
public func pyUniversalNewlines(_ s: String) -> String {
    var out = String.UnicodeScalarView()
    let scalars = Array(s.unicodeScalars)
    var i = 0
    while i < scalars.count {
        let c = scalars[i]
        if c.value == 0x0D {
            out.append(Unicode.Scalar(0x0A))
            if i + 1 < scalars.count, scalars[i + 1].value == 0x0A { i += 1 }
        } else {
            out.append(c)
        }
        i += 1
    }
    return String(out)
}

// MARK: - str.splitlines / str.split()

/// Python's `str.splitlines()` boundary set — wider than `\n`/`\r\n`: it also
/// breaks on VT, FF, the three ASCII separators U+001C..U+001E, NEL and the
/// Unicode line/paragraph separators, and it yields no trailing empty piece.
///
/// Scalar-by-scalar, NOT by `Character`: swift reads CRLF as one grapheme, so a
/// `Character`-based split cannot see the pair python treats as one boundary
/// while `\r` alone is another.
public func pySplitlines(_ s: String) -> [String] {
    var out: [String] = []
    var cur = String.UnicodeScalarView()
    let scalars = Array(s.unicodeScalars)
    var i = 0
    while i < scalars.count {
        let c = scalars[i]
        let isBreak: Bool
        switch c.value {
        case 0x0A, 0x0B, 0x0C, 0x0D, 0x1C, 0x1D, 0x1E, 0x85, 0x2028, 0x2029: isBreak = true
        default: isBreak = false
        }
        if isBreak {
            out.append(String(cur))
            cur = String.UnicodeScalarView()
            if c.value == 0x0D, i + 1 < scalars.count, scalars[i + 1].value == 0x0A { i += 1 }
        } else {
            cur.append(c)
        }
        i += 1
    }
    if !cur.isEmpty { out.append(String(cur)) }
    return out
}

/// Python's `str.split()` with NO separator: runs of `str.isspace()` scalars are
/// the separator, leading and trailing runs are dropped, and no empty piece is
/// produced. `' '.join(x.split())` is python's whitespace collapse.
public func pySplitWhitespace(_ s: String) -> [String] {
    var out: [String] = []
    var cur = String.UnicodeScalarView()
    for c in s.unicodeScalars {
        if pyIsSpace(c) {
            if !cur.isEmpty {
                out.append(String(cur))
                cur = String.UnicodeScalarView()
            }
        } else {
            cur.append(c)
        }
    }
    if !cur.isEmpty { out.append(String(cur)) }
    return out
}
