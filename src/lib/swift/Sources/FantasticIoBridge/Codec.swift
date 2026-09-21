// codec — the binary-safe frame codec shared by every transport.
//
// Mirror of py `io/io_bridge/_codec.py` + rust `fantastic-io-bridge::codec`. A
// frame whose payload carries raw bytes (a `read_stream` chunk) is a BINARY
// frame: `[ 4-byte BE u32 H | H-byte JSON header | M-byte raw body ]`. The
// header is the envelope with the bytes value nulled + a `_binary_path` naming
// where it lived; the receiver places the trailing M bytes there. A frame with
// no bytes is plain UTF-8 JSON. This is how raw bytes ride the wire WITHOUT
// base64. Swift's `JSON` can't hold raw `Data`, so (like rust's `Value`) the
// header + body travel SEPARATELY — the wire layout is identical, so a swift leg
// and a python/rust web_ws interoperate byte-for-byte. The text/binary split is
// carried by the TRANSPORT — every transport is WS-based (web_ws, ws_bridge,
// relay_connector over the relay) and uses the WS frame type; the relay forwards
// the frame kind end-to-end.

import FantasticJSON
import Foundation

public enum Codec {
    /// python `encode_frame`'s TEXT half — `json.dumps(envelope, default=str)`,
    /// i.e. `', '`/`': '` separators and `ensure_ascii=True`. Every
    /// server→client text frame (web_ws replies/events, the ws + relay legs,
    /// the exec relay) goes through here; `serialize()` sent compact raw UTF-8,
    /// so `·` and `↑` crossed the wire unescaped.
    public static func encodeText(_ envelope: JSON) -> String { pyJsonDumps(envelope) }

    /// Encode a binary frame `[4B len | header | body]`. `header` should already
    /// carry `_binary_path` (and null at that path) per the convention.
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

    /// python `set_path(obj, path, value)` — walk the dotted path and assign at
    /// the last segment. A list holder indexes with `int(part)` (negative
    /// indices resolve from the end, as python's do); an object holder keys with
    /// the raw segment.
    ///
    /// Every python failure — a missing intermediate key, a non-int index into a
    /// list, an index out of range, a scalar where a container was expected —
    /// raises out of `set_path`, and the ws loop turns that into the ONE frame
    /// error (A11.7). So this throws for each of them rather than silently
    /// creating what was not there.
    public static func setPath(_ obj: inout JSON, _ path: String, _ value: JSON) throws(FrameError)
    {
        let parts = path.split(separator: ".", omittingEmptySubsequences: false).map(String.init)
        try assign(&obj, parts, value)
    }

    private static func assign(_ obj: inout JSON, _ parts: [String], _ value: JSON)
        throws(FrameError)
    {
        guard let head = parts.first else { throw FrameError("empty path") }
        let rest = Array(parts.dropFirst())
        switch obj {
        case .array(var items):
            guard let raw = Int(head) else {
                throw FrameError("list indices must be integers or slices, not str")
            }
            let idx = raw < 0 ? items.count + raw : raw
            guard idx >= 0, idx < items.count else {
                throw FrameError("list assignment index out of range")
            }
            if rest.isEmpty {
                items[idx] = value
            } else {
                var child = items[idx]
                try assign(&child, rest, value)
                items[idx] = child
            }
            obj = .array(items)
        case .object(var members):
            if rest.isEmpty {
                members[head] = value
                obj = .object(members)
                return
            }
            // python raises `KeyError` on a missing intermediate — it does NOT
            // create one. A frame naming `payload.nope.deep` is malformed.
            guard var child = members[head] else { throw FrameError("\(pyRepr(head))") }
            try assign(&child, rest, value)
            members[head] = child
            obj = .object(members)
        default:
            // python: `'int' object does not support item assignment` and
            // friends — a scalar is not a holder.
            throw FrameError("object does not support item assignment")
        }
    }

    /// Decode a binary frame → `(header, body)`.
    ///
    /// A frame arrives from a PEER, so every length in it is hostile until
    /// checked: a 4-byte prefix claiming 4 GiB of header on a 3-byte frame would
    /// slice garbage. The bounds are checked here, once, and every malformed
    /// frame leaves as a `FrameError` CARRYING WHY — python's text. It used to
    /// return `nil`, so the one caller that has to name the fault (the ws read
    /// loop's skip line) had nothing to name it with.
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
        do {
            return (try JSON.parse(headerStr), body)
        } catch {
            throw FrameError("bad binary frame header: \(error)")
        }
    }
}
