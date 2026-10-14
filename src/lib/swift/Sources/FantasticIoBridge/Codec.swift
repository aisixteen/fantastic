// codec — the binary-safe frame codec shared by every transport.
//
// Mirror of py `io/io_bridge/_codec.py` + rust `fantastic-io-bridge::codec`. A
// frame whose payload carries raw bytes (a `read_stream` chunk) is a BINARY
// frame: `[ 4-byte BE u32 H | H-byte JSON header | M-byte raw body ]`. The
// header is the envelope with the bytes value nulled + a `_binary_path` naming
// where it lived; the receiver places the trailing M bytes there. A frame with
// no bytes is plain UTF-8 JSON. This is how raw bytes ride the wire WITHOUT
// base64. Swift's `JSON` can't hold raw `Data`, so (like rust's `Value`) the
// header + body travel SEPARATELY, the body with the path it sits at (the
// kernel's `Blob`) — the wire layout is identical, so a swift leg and a
// python/rust web_ws interoperate byte-for-byte. Python's `set_path` is the
// kernel's `pySetPath`.
//
// `_binary_path` is a JSON ARRAY of steps from the envelope root: a STRING step
// is an object key, taken literally (a key may hold `.`); an INTEGER step is a
// list index. `["payload", "bytes"]`, `["payload", "a.b"]` (the top-level
// payload key `"a.b"`), `["data", "items", 0, "data"]`. A dotted string could
// not tell the key `"a.b"` from the nested `a` → `b`. The text/binary split is
// carried by the TRANSPORT — every transport is WS-based (web_ws, ws_bridge,
// relay_connector over the relay) and uses the WS frame type; the relay forwards
// the frame kind end-to-end.

import FantasticJSON
import FantasticKernel
import Foundation

/// THE ONE FRAME CAP every wire leg holds: 16 MiB — a WS frame (`ws_bridge`'s
/// max frame, the relay's `MAX_FRAME`, the web host's websocket `maxFrameSize`
/// under `web_ws`), an exec_bridge control-fd frame, and a web_rest request body
/// (the http face of the same envelope). Larger data moves in chunks
/// (`write_stream`). Python `io_bridge.MAX_FRAME`.
public let MAX_FRAME = 16 * 1024 * 1024

public enum Codec {
    /// python `encode_frame`'s TEXT half — `json.dumps(envelope, default=str)`,
    /// i.e. `', '`/`': '` separators and `ensure_ascii=True`. Every
    /// server→client text frame (web_ws replies/events, the ws + relay legs,
    /// the exec relay) goes through here; `serialize()` sent compact raw UTF-8,
    /// so `·` and `↑` crossed the wire unescaped.
    public static func encodeText(_ envelope: JSON) -> String { pyJsonDumps(envelope) }

    /// Encode a binary frame `[4B len | header | body]`. `header` should already
    /// carry `_binary_path` (and null at that path) per the convention —
    /// `binaryPath(_:)` spells it.
    public static func encodeBinaryFrame(header: JSON, body: Data) -> Data {
        let head = (encodeText(header).data(using: .utf8)) ?? Data("{}".utf8)
        var out = Data(capacity: 4 + head.count + body.count)
        var len = UInt32(head.count).bigEndian
        withUnsafeBytes(of: &len) { out.append(contentsOf: $0) }
        out.append(head)
        out.append(body)
        return out
    }

    /// python `decode_frame`'s `ValueError` — one error type for every malformed
    /// frame, so a read loop catches ONE thing and keeps its socket.
    public struct FrameError: Error, CustomStringConvertible, Sendable, Equatable {
        public let message: String
        public init(_ message: String) { self.message = message }
        public var description: String { message }
    }

    /// `_binary_path` as the wire spells it: the steps from the envelope root, a
    /// JSON array — a key as a string, a list index as an integer.
    public static func binaryPath(_ steps: [PathStep]) -> JSON {
        .array(steps.map(\.json))
    }

    /// `_binary_path` for a body at `parts` of the envelope's `payload` — the
    /// `payload` step, then the body's own steps in it.
    public static func payloadPath(_ parts: [PathStep]) -> JSON {
        binaryPath(["payload"] + parts)
    }

    /// python `decode_frame`'s placement, in swift's carrier: pop `_binary_path`
    /// off a binary frame's header and put the body's stand-in, `null`, in the
    /// slot it names (swift's `JSON` holds no bytes; the body rides beside it).
    /// Returns the slot's steps, or `nil` when the header names none — absent or
    /// `null`, where python places no body. A path python's `set_path` cannot
    /// place is the frame's error: `bad _binary_path <path>: <why>`, a
    /// dotted string included (`not a list of steps`).
    public static func placeBody(_ header: inout JSON) throws(FrameError) -> [PathStep]? {
        guard case .object(var members) = header,
            let raw = members.removeValue(forKey: "_binary_path")
        else { return nil }
        header = .object(members)
        if raw.isNull { return nil }
        do {
            return try pySetPath(&header, wire: raw, .null)
        } catch {
            throw FrameError("bad _binary_path \(pyRepr(raw)): \(error.message)")
        }
    }

    /// The slot a binary frame's header names, checked exactly as `placeBody`
    /// checks it; the header itself is left as it is.
    public static func bodySlot(_ header: JSON) throws(FrameError) -> [PathStep]? {
        var h = header
        return try placeBody(&h)
    }

    /// The body as the frame's `payload` holds it: `Blob` at its steps INSIDE the
    /// payload when the header's `_binary_path` names a slot under `payload`,
    /// else `nil` — a body the frame put anywhere else is not part of the payload
    /// python's decoder hands on. The header was checked when its frame was
    /// decoded (`decodeBinaryFrame`) or built here, so a slot it cannot place
    /// does not reach this.
    public static func payloadBody(_ header: JSON, body: Data) -> Blob? {
        bodyUnder("payload", header, body: body)
    }

    /// The body as the frame's `key` holds it — `payload` for a call or an
    /// event, `data` for a reply: `Blob` at its steps INSIDE that value when the
    /// header's `_binary_path` names a slot under it, else `nil` (a body the
    /// frame put anywhere else is not part of that value). `["data", "x.y"]` is
    /// the reply key `"x.y"`, never `x` → `y`.
    public static func bodyUnder(_ key: String, _ header: JSON, body: Data) -> Blob? {
        guard let steps = try? bodySlot(header) else { return nil }
        return Blob(parts: steps, body).under(key)
    }

    /// A frame that carries `body` at its slot (`body.parts`, from the envelope
    /// root): `null` put in that slot of `envelope` in python's position —
    /// appended when the envelope did not name it — and `_binary_path` naming
    /// it. What every leg sends as `[4B len | header | body.bytes]`.
    public static func binaryHead(_ envelope: JSON, _ body: Blob) -> JSON {
        var head = envelope
        // admitted where it was made: the slot is one of the envelope's
        try? pySetPath(&head, parts: body.parts, .null)
        if case .object(var o) = head {
            o["_binary_path"] = binaryPath(body.parts)
            head = .object(o)
        }
        return head
    }

    /// python `decode_frame`'s placement of a binary call's body, in swift's
    /// carrier. Python sets the body at `_binary_path` and hands the frame's
    /// `payload` to `kernel.send`; swift's `JSON` holds no bytes, so a body under
    /// the frame's `payload` — at ANY slot there (`["payload", "data"]` for
    /// `paste_image`, `["payload", "bytes"]` for `write_stream`, the literal key
    /// `["payload", "a.b"]`) — becomes the item's binary AT ITS STEPS inside the
    /// payload (`Blob`), and the payload keeps what the sender left in that slot
    /// (`null`, in python's position). A body the frame put anywhere else is not
    /// in the payload python's decoder hands the kernel (`binary` nil).
    public static func callPayload(_ envelope: JSON, body: Data) -> (
        payload: JSON, binary: Blob?
    ) {
        (envelope.asObject?["payload"] ?? .object([:]), payloadBody(envelope, body: body))
    }

    /// Decode a binary frame → `(header, body)`, the header as it was sent
    /// (`_binary_path` still in it).
    ///
    /// A frame arrives from a PEER, so every length in it is hostile until
    /// checked: a 4-byte prefix claiming 4 GiB of header on a 3-byte frame would
    /// slice garbage. The bounds are checked here, once, and every malformed
    /// frame leaves as a `FrameError` CARRYING WHY — python's text, so the one caller that has to name the fault (the ws
    /// read loop's skip line) can name it. So is the header's
    /// `_binary_path`: one python's `set_path` cannot place (`placeBody`) makes
    /// the frame malformed here, before any caller reads it.
    public static func decodeBinaryFrame(_ data: Data) throws(FrameError) -> (
        header: JSON, body: Data
    ) {
        guard data.count >= 4 else {
            throw FrameError("binary frame shorter than its 4-byte header (\(data.count)B)")
        }
        let h =
            (UInt32(data[data.startIndex]) << 24) | (UInt32(data[data.startIndex + 1]) << 16)
            | (UInt32(data[data.startIndex + 2]) << 8) | UInt32(data[data.startIndex + 3])
        let headerLen = Int(h)
        guard data.count >= 4 + headerLen else {
            throw FrameError(
                "binary frame header claims \(headerLen)B but only "
                    + "\(data.count - 4)B follow the length prefix")
        }
        let headerData = data.subdata(in: (data.startIndex + 4)..<(data.startIndex + 4 + headerLen))
        let body = data.subdata(in: (data.startIndex + 4 + headerLen)..<data.endIndex)
        guard let headerStr = String(data: headerData, encoding: .utf8) else {
            throw FrameError("bad binary frame header: 'utf-8' codec can't decode bytes")
        }
        let header: JSON
        do {
            header = try JSON.parseStrict(headerStr)
        } catch {
            throw FrameError("bad binary frame header: \(error)")
        }
        _ = try bodySlot(header)
        return (header, body)
    }
}
