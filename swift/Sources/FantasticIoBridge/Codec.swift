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
    /// Encode a binary frame `[4B len | header | body]`. `header` should already
    /// carry `_binary_path` (and null at that path) per the convention.
    public static func encodeBinaryFrame(header: JSON, body: Data) -> Data {
        let head = (header.serialize().data(using: .utf8)) ?? Data("{}".utf8)
        var out = Data(capacity: 4 + head.count + body.count)
        var len = UInt32(head.count).bigEndian
        withUnsafeBytes(of: &len) { out.append(contentsOf: $0) }
        out.append(head)
        out.append(body)
        return out
    }

    /// Decode a binary frame → `(header, body)`. Errors on a short/malformed frame.
    public static func decodeBinaryFrame(_ data: Data) -> (header: JSON, body: Data)? {
        guard data.count >= 4 else { return nil }
        let h =
            (UInt32(data[data.startIndex]) << 24) | (UInt32(data[data.startIndex + 1]) << 16)
            | (UInt32(data[data.startIndex + 2]) << 8) | UInt32(data[data.startIndex + 3])
        let headerLen = Int(h)
        guard data.count >= 4 + headerLen else { return nil }
        let headerData = data.subdata(in: (data.startIndex + 4)..<(data.startIndex + 4 + headerLen))
        let body = data.subdata(in: (data.startIndex + 4 + headerLen)..<data.endIndex)
        guard let headerStr = String(data: headerData, encoding: .utf8),
            let header = try? JSON.parse(headerStr)
        else { return nil }
        return (header, body)
    }
}
