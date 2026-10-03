// SECRET VALUES on a call — the caller marks them, the kernel never records them.
//
// Port of python `kernel/_secret.py` (canonical, owner 2026-10-01).
//
// A batch item may carry `secret: [mark, …]` beside `target`/`payload`/`timeout`.
// A mark is rooted at the item: `payload` + a field of the payload, `result` (the
// whole reply), or `result` + a field of it. The TARGET is handed the real payload
// and the CALLER the real reply; every place the kernel RECORDS or SHOWS the call
// carries `<secret>` instead — the call and settle rows (and so the memory store),
// the `ledger` read-back and dedup, the state stream's `payload` and `summary`, the
// inbox and watcher mirrors, and any error text the call's outcome carries.
//
// A mark is spelled one of two ways (one list may mix them):
//   * a DOTTED STRING — `payload.key`, `result`, `result.fields.api_key` — split on
//     its dots; at a list, a step of ASCII digits is an index;
//   * an ARRAY OF STEPS (owner, 2026-10-03) — `["payload", "a.b"]`,
//     `["result", "items", 0]` — exact, with the step rules of the binary frame's
//     `_binary_path` (`PathStep`): a STRING step is an object key taken literally
//     (it may hold a `.`), an INTEGER step (non-negative) a list index. It is the
//     only way to mark a key that holds a dot.
//
// Two limits, by design:
//   * masking is not isolation — the receiver holds the value; send secrets to a
//     bundle that keeps them, never to an AI agent (they would enter its prompt and
//     its memory);
//   * a call whose record holds placeholders cannot be answered FROM that record:
//     re-sending a named execution with secrets after it left RAM is refused, loud.
//
// Not secret-able: `payload.type` — the verb is how every row is read; and any
// payload field of `create_agent`/`update_agent` — the kernel writes those into the
// agent's RECORD (its `agent_created`/`agent_updated` events, `agent.yaml`), where
// no mark can hold. A value an agent must keep belongs in a `credential_store`, its
// record naming the ref (owner-approved 2026-10-03, audit S2).
//
// RAW BYTES. Python holds a binary item's body as a `bytes` value inside the
// payload dict (at its path: `data` for `paste_image`, `bytes` for
// `write_stream`), and a binary reply's chunk at its key of the reply dict. Swift
// carries those bytes beside the JSON (`Item.binary` and `Outcome.body` — each a
// `Blob` naming its path), so every path walk here sees them where python does:
// `payload.<its path>` names the item's body, `result.<its path>` the reply's
// (`result.bytes` for a `read_stream` chunk). A body's secret VALUE is what
// python's `str(bytes)` spells — its `b'…'` repr.

import FantasticJSON
import Foundation
import OrderedCollections

/// One mark below its root (`payload` / `result`): the steps, and whether they
/// are EXACT (an array mark: a key step is only an object key, an index step only
/// a list index) or DOTTED (every step a key — at a list, an index when it is
/// ASCII digits). Python `_Path`.
struct MarkPath: Sendable, Equatable {
    let steps: [PathStep]
    let exact: Bool
}

/// What one item asked to be kept: the payload/result paths, the marks as given
/// (recorded on the call row — marks are not secret), and the payload's secret
/// VALUES (RAM only — scrubbed out of every recorded string).
public struct Secret: Sendable {
    /// What every record and mirror carries in a secret value's place.
    public static let placeholder = "<secret>"

    /// The verbs whose every payload field (`type` aside) lands in an agent's
    /// record — the id, the handler, the placement, the meta. A mark there is
    /// refused (`check`). Python `_RECORD_VERBS`.
    static let recordVerbs: Set<String> = ["create_agent", "update_agent"]

    /// The marks AS GIVEN — dotted strings and/or arrays of steps — the call
    /// row's `secret` field, and what the handler reads back as
    /// `KernelTaskLocals.currentSecret`.
    public let paths: [JSON]
    let payloadPaths: [MarkPath]
    let resultPaths: [MarkPath]
    let values: [String]

    /// Validate an item's `secret` (python `check`). Absent (or JSON null) ⇒
    /// nothing secret. A payload mark the payload does not have is refused — a
    /// typo would otherwise record the value it meant to hide.
    static func check(_ raw: JSON?, payload: JSON, blob: Blob?) -> LaunchField<Secret?> {
        guard let raw, raw != .null else { return .ok(nil) }
        func bad(_ why: String) -> LaunchField<Secret?> {
            .refused(
                Refusal.make(
                    Refusal.badArg, "send: secret \(why)", extra: [("arg", .string("secret"))]))
        }
        guard case .array(let list) = raw, !list.isEmpty else {
            return bad("must be a non-empty list of paths, got \(pyRepr(raw))")
        }
        let carried = Carried(json: payload, blob: blob)
        var payloadPaths: [MarkPath] = []
        var resultPaths: [MarkPath] = []
        let recordVerb = payload["type"].asString.flatMap { recordVerbs.contains($0) ? $0 : nil }
        for p in list {
            let parsed: Result<(root: String, path: MarkPath), MarkError>
            switch p {
            case .string(let s): parsed = dottedMark(s)
            case .array(let a): parsed = arrayMark(a)
            default:
                return bad("path must be a dotted string or an array of steps, got \(pyRepr(p))")
            }
            let root: String
            let path: MarkPath
            switch parsed {
            case .failure(let why): return bad(why.text)
            case .success(let r): (root, path) = r
            }
            if root == "result" {
                resultPaths.append(path)
                continue
            }
            if path.steps == [.key("type")] {
                return bad("cannot hide payload.type — the verb is how every row is read")
            }
            if let verb = recordVerb {
                return bad(
                    "path \(pyRepr(p)) names a field \(verb) writes into the agent record: a "
                        + "record field cannot be secret — use a credential_store ref")
            }
            if carried.get(path) == nil {
                return bad("path \(pyRepr(p)) names nothing in the payload")
            }
            payloadPaths.append(path)
        }
        let values = payloadPaths.flatMap { kp in carried.get(kp).map(secretStrings) ?? [] }
        // the marks AS GIVEN — what the call row records and what the handler
        // reads back as `currentSecret`
        return .ok(
            Secret(
                paths: list, payloadPaths: payloadPaths, resultPaths: resultPaths, values: values))
    }

    /// The payload as recorded and shown: secret fields → placeholder, then every
    /// secret value scrubbed out of every string (python `shown_payload`). A
    /// hidden body (`payload.<its path>`) drops the blob — its slot holds the
    /// placeholder, and the shown payload is text.
    func shownPayload(_ payload: JSON, blob: Blob?) -> ShownPayload {
        var out = Carried(json: payload, blob: blob)
        for kp in payloadPaths { out = out.put(kp) }
        return ShownPayload(json: Secret.scrub(out.json, values), blob: out.blob)
    }

    /// An outcome's RESULT as recorded and shown: secret result fields →
    /// placeholder, and every secret value (the payload's and the result's own)
    /// scrubbed from its text (python `shown_outcome`). The result ONLY: `status`,
    /// `pid` and the rest are the kernel's own words — a short secret must not
    /// mangle "completed" into "compl<secret>t<secret>d".
    func shownOutcome(result: JSON, body: Blob?) -> ShownOutcome {
        // a reply's chunk sits at ITS slot of the reply (`Outcome.body`)
        var res = Carried(json: result, blob: body)
        var vals = values
        for kp in resultPaths {
            if let v = res.get(kp) { vals += secretStrings(v) }
            res = res.put(kp)
        }
        return ShownOutcome(result: Secret.scrub(res.json, vals), body: res.blob)
    }

    /// The outcome a JOINER that is not the execution's original caller is
    /// handed: the result as shown (`shownOutcome`), every other field — status,
    /// pid, attempts — the kernel's own words, unchanged. Python
    /// `shown_outcome(recorded, secret)` on the join (audit 2026-10-03, F1).
    func shownToJoiner(_ o: Outcome) -> Outcome {
        let s = shownOutcome(result: o.result, body: o.body)
        return Outcome(pid: o.pid, status: o.status, result: s.result, attempts: o.attempts, body: s.body)
    }

    /// The answer to re-sending an execution whose record holds placeholders
    /// (python `replay_refused`). `key` is the caller's name or the int id.
    static func replayRefused(_ key: JSON) -> JSON {
        Refusal.make(
            Refusal.badArg,
            "send: execution \(pyRepr(key)) carried secrets — its record holds "
                + "\(placeholder) placeholders, not values, so it cannot be answered from "
                + "the record; send again under a new id",
            extra: [("arg", .string("id"))])
    }

    /// Every occurrence of a secret value inside any string of `x` → placeholder —
    /// an error text that quotes the key must not carry it into a row or a log
    /// (python `scrub`). A dict KEY, a NUMBER and a BOOL whose text — python
    /// `str(x)`, `True`/`False` for a bool — EQUALS a secret value are the
    /// placeholder too: a reply keyed by the token it was given, a PIN echoed as an
    /// int (owner-approved 2026-10-03, audit S7). Equal, not contained: a key is
    /// structure, and `type` must survive a one-letter secret. Two keys that scrub
    /// alike keep the later value.
    static func scrub(_ x: JSON, _ values: [String]) -> JSON {
        if values.isEmpty { return x }
        return scrubOrdered(x, longestFirst(values))
    }

    private static func scrubOrdered(_ x: JSON, _ order: [[Unicode.Scalar]]) -> JSON {
        switch x {
        case .string(let s): return .string(scrubText(s, order))
        case .object(let o):
            var out = OrderedDictionary<String, JSON>()
            for (k, v) in o {
                out[isSecretText(k, order) ? placeholder : k] = scrubOrdered(v, order)
            }
            return .object(out)
        case .array(let a): return .array(a.map { scrubOrdered($0, order) })
        case .integer(let n): return isSecretText(String(n), order) ? .string(placeholder) : x
        case .double(let d): return isSecretText(pyFloatRepr(d), order) ? .string(placeholder) : x
        case .bool(let b): return isSecretText(b ? "True" : "False", order) ? .string(placeholder) : x
        default: return x
        }
    }

    /// A key's or a number's text IS a secret value (equal, not contained).
    private static func isSecretText(_ text: String, _ order: [[Unicode.Scalar]]) -> Bool {
        order.contains(Array(text.unicodeScalars))
    }

    /// python `sorted(values, key=len, reverse=True)`: longest (in CODE POINTS)
    /// first, ties kept in their given order — python's reverse sort is stable.
    private static func longestFirst(_ values: [String]) -> [[Unicode.Scalar]] {
        values.map { Array($0.unicodeScalars) }.enumerated()
            .sorted {
                $0.element.count != $1.element.count
                    ? $0.element.count > $1.element.count : $0.offset < $1.offset
            }
            .map(\.element)
    }

    /// python `for v in …: if v in x: x = x.replace(v, PLACEHOLDER)` — each value
    /// in turn, every non-overlapping occurrence left to right, on code points.
    private static func scrubText(_ s: String, _ order: [[Unicode.Scalar]]) -> String {
        var hay = Array(s.unicodeScalars)
        let mark = Array(placeholder.unicodeScalars)
        for needle in order where !needle.isEmpty && needle.count <= hay.count {
            var out: [Unicode.Scalar] = []
            out.reserveCapacity(hay.count)
            var i = 0
            var hit = false
            while i + needle.count <= hay.count {
                if hay[i..<(i + needle.count)].elementsEqual(needle) {
                    out.append(contentsOf: mark)
                    i += needle.count
                    hit = true
                } else {
                    out.append(hay[i])
                    i += 1
                }
            }
            if hit {
                out.append(contentsOf: hay[i...])
                hay = out
            }
        }
        var view = String.UnicodeScalarView()
        view.append(contentsOf: hay)
        return String(view)
    }
}

/// Why a mark is malformed — the `<why>` of `send: secret <why>`.
struct MarkError: Error {
    let text: String
    init(_ text: String) { self.text = text }
}

/// A dotted-string mark → (root, path below it), or why not. Python `_dotted`.
private func dottedMark(_ p: String) -> Result<(root: String, path: MarkPath), MarkError> {
    if p.isEmpty { return .failure(MarkError("path must be a non-empty string, got \(pyRepr(p))")) }
    // python `p.split(".")` — on the code point, never a grapheme.
    let keys = p.unicodeScalars.split(separator: ".", omittingEmptySubsequences: false)
        .map { String(String.UnicodeScalarView($0)) }
    if keys.contains(where: { $0.isEmpty }) {
        return .failure(MarkError("path \(pyRepr(p)) has an empty step"))
    }
    let below = MarkPath(steps: keys.dropFirst().map { .key($0) }, exact: false)
    if keys[0] == "result" { return .success(("result", below)) }
    if keys[0] == "payload" && keys.count > 1 { return .success(("payload", below)) }
    return .failure(MarkError("path \(pyRepr(p)) must start with 'payload.' or be 'result[.…]'"))
}

/// An array-of-steps mark → (root, path below it), or why not. Each step is a
/// string (an object key, taken literally) or a non-negative integer (a list
/// index) — a bool, a float, a negative integer, null, a list or an object is
/// refused, the first one named. Python `_array`.
private func arrayMark(_ p: [JSON]) -> Result<(root: String, path: MarkPath), MarkError> {
    if p.isEmpty { return .failure(MarkError("path [] has no steps")) }
    var steps: [PathStep] = []
    for s in p {
        switch s {
        case .string(let k): steps.append(.key(k))
        case .integer(let n) where n >= 0: steps.append(.index(Int(n)))
        default:
            return .failure(
                MarkError(
                    "path \(pyRepr(JSON.array(p))) step \(pyRepr(s)) is neither a string (an "
                        + "object key) nor a non-negative integer (a list index)"))
        }
    }
    let below = MarkPath(steps: Array(steps.dropFirst()), exact: true)
    if steps[0] == .key("result") { return .success(("result", below)) }
    if steps[0] == .key("payload") && steps.count > 1 { return .success(("payload", below)) }
    return .failure(
        MarkError(
            "path \(pyRepr(JSON.array(p))) must start with 'payload' and a step under it, or "
                + "with 'result'"))
}

/// A payload as recorded and shown. `blob` is the item's raw body (at its path)
/// when it is NOT secret (a watcher still receives it, as python's mirror carries
/// the bytes); `nil` when there was none or `payload.<its path>` hid it.
struct ShownPayload: Sendable {
    let json: JSON
    let blob: Blob?

    /// The fanout element — python publishes the shown dict, bytes and all.
    var inboxItem: InboxItem {
        blob.map { Kernel.binaryItem(json, $0) } ?? .text(json)
    }
}

/// An outcome's result as recorded and shown — what `ledger` reads back (python
/// `shown_out`; its `status` is the outcome's own, never scrubbed). `body` is the
/// reply chunk, at its slot, when it is NOT secret.
struct ShownOutcome: Sendable {
    let result: JSON
    let body: Blob?
}

// ─── the walk ────────────────────────────────────────────────────────────────

/// What a path names: a JSON value, the raw blob, or the whole carried value.
private enum Found {
    case json(JSON)
    case blob(Data)
    case whole(Carried)
}

/// A value as python holds it: the JSON, plus — for a binary item or reply — the
/// raw bytes python keeps inside it, at the blob's path.
private struct Carried {
    var json: JSON
    var blob: Blob?

    /// python `_get`: `nil` when the path names nothing. No steps = the whole.
    /// The blob sits at its path when the value is a dict, as python's bytes do:
    /// a mark that walks those steps (each by `matches`) names the raw body.
    func get(_ path: MarkPath) -> Found? {
        let keys = path.steps
        if keys.isEmpty { return .whole(self) }
        if let blob, case .object = json, keys.count >= blob.parts.count,
            zip(keys, blob.parts).allSatisfy({ matches($0, $1, exact: path.exact) })
        {
            // `_step(bytes, k)` — raw bytes have no fields.
            return keys.count == blob.parts.count ? .blob(blob.bytes) : nil
        }
        var cur = json
        for k in keys {
            guard let next = stepInto(cur, k, exact: path.exact) else { return nil }
            cur = next
        }
        return .json(cur)
    }

    /// python `_put`: the value at `path` → placeholder (a missing path leaves it
    /// as is); no steps replace the whole value.
    func put(_ path: MarkPath) -> Carried {
        if path.steps.isEmpty { return Carried(json: .string(Secret.placeholder), blob: nil) }
        guard let found = get(path) else { return self }
        if case .blob = found, let blob {
            // in place when the header held the slot, appended otherwise
            var j = json
            try? pySetPath(&j, parts: blob.parts, .string(Secret.placeholder))
            return Carried(json: j, blob: nil)
        }
        return Carried(json: putInto(json, path.steps[...]), blob: blob)
    }
}

/// Whether a mark's step walks the blob's `part` — python `_step` at the
/// container that holds the bytes: a key is the same key; an index is the same
/// integer step (exact) or a digits key spelling it (dotted).
private func matches(_ step: PathStep, _ part: PathStep, exact: Bool) -> Bool {
    switch (step, part) {
    case (.key(let a), .key(let b)): return a == b
    case (.index(let a), .index(let b)): return exact && a == b
    case (.key(let k), .index(let i)): return !exact && digitIndex(k) == i
    case (.index, .key): return false
    }
}

/// A dotted step's list index: ASCII digits only (python `key.isascii() and
/// key.isdigit()`), else `nil`.
private func digitIndex(_ key: String) -> Int? {
    guard !key.isEmpty, key.unicodeScalars.allSatisfy({ $0.value >= 0x30 && $0.value <= 0x39 })
    else { return nil }
    return Int(key)
}

/// python `_step`: at a dict a key step (present even when its value is null) —
/// an exact integer step names nothing there; at a list an index — an exact
/// integer step, or a dotted step spelled in ASCII digits (an exact string step
/// names nothing there).
private func stepInto(_ cur: JSON, _ step: PathStep, exact: Bool) -> JSON? {
    switch (cur, step) {
    case (.object(let o), .key(let k)):
        return o[k]
    case (.array(let a), .index(let i)):
        guard exact, i < a.count else { return nil }
        return a[i]
    case (.array(let a), .key(let k)):
        guard !exact, let i = digitIndex(k), i < a.count else { return nil }
        return a[i]
    default:
        return nil
    }
}

/// The JSON half of `_put`, the path already known to exist.
private func putInto(_ x: JSON, _ steps: ArraySlice<PathStep>) -> JSON {
    guard let step = steps.first else { return .string(Secret.placeholder) }
    switch (x, step) {
    case (.object(var o), .key(let k)):
        guard let v = o[k] else { return x }
        o[k] = putInto(v, steps.dropFirst())
        return .object(o)
    case (.array(var a), _):
        let at: Int?
        switch step {
        case .index(let i): at = i
        case .key(let k): at = digitIndex(k)
        }
        guard let i = at, i < a.count else { return x }
        a[i] = putInto(a[i], steps.dropFirst())
        return .array(a)
    default:
        return x
    }
}

/// python `_strings`: every string a secret value holds — the value itself, or
/// its leaves; a number as `str(x)`; nothing for None or a bool.
private func secretStrings(_ f: Found) -> [String] {
    switch f {
    case .json(let j): return jsonStrings(j)
    case .blob(let d): return [pyBytesRepr(d)]
    case .whole(let c):
        // only a whole RESULT is markable (`result`): the body is the reply's
        // chunk at ITS slot — in the slot's place (appended last when the value
        // has no key there yet, where `Kernel.storeValue` puts it). A body with no
        // slot in the value is not part of it. Rust `strings_with_body`.
        guard let blob = c.blob else { return jsonStrings(c.json) }
        var placed = c.json
        do {
            try pySetPath(&placed, parts: blob.parts, .null)
        } catch {
            return jsonStrings(c.json)
        }
        return stringsAlong(placed, blob.parts[...], blob.bytes)
    }
}

/// `jsonStrings` of `x` with the body's bytes standing at the steps `at`.
private func stringsAlong(_ x: JSON, _ at: ArraySlice<PathStep>, _ bytes: Data) -> [String] {
    guard let first = at.first else { return [pyBytesRepr(bytes)] }
    let rest = at.dropFirst()
    switch (x, first) {
    case (.object(let o), .key(let key)):
        return o.flatMap { k, v in k == key ? stringsAlong(v, rest, bytes) : jsonStrings(v) }
    case (.array(let a), .index(let i)):
        return a.enumerated().flatMap { j, v in
            j == i ? stringsAlong(v, rest, bytes) : jsonStrings(v)
        }
    default:
        return jsonStrings(x)
    }
}

private func jsonStrings(_ x: JSON) -> [String] {
    switch x {
    case .string(let s): return s.isEmpty ? [] : [s]
    case .object(let o): return o.values.flatMap(jsonStrings)
    case .array(let a): return a.flatMap(jsonStrings)
    case .null, .bool: return []
    case .integer(let n): return [String(n)]
    case .double(let d): return [pyFloatRepr(d)]
    }
}

/// python `str(bytes)` / `repr(bytes)`: `b'…'`, single-quoted unless the bytes
/// hold a `'` and no `"`; the quote and `\` escaped; TAB/LF/CR as `\t` `\n` `\r`;
/// every other byte outside `0x20..<0x7f` as `\xhh`.
func pyBytesRepr(_ d: Data) -> String {
    let quote: UInt8 = (d.contains(0x27) && !d.contains(0x22)) ? 0x22 : 0x27
    var out = "b" + String(UnicodeScalar(quote))
    for b in d {
        switch b {
        case quote, 0x5C: out += "\\" + String(UnicodeScalar(b))
        case 0x09: out += "\\t"
        case 0x0A: out += "\\n"
        case 0x0D: out += "\\r"
        case 0x20..<0x7F: out += String(UnicodeScalar(b))
        default:
            let hex = String(b, radix: 16)
            out += "\\x" + (hex.count < 2 ? "0" + hex : hex)
        }
    }
    return out + String(UnicodeScalar(quote))
}
