// The host serves websocket MESSAGES, not frames.
//
// Python's websockets server and rust's tungstenite reassemble, bound and
// validate; so does the host: fragments are reassembled up to io_bridge's `MAX_FRAME`, and
// the socket closes with python's code and empty reason: 1007 for a text message
// that is not UTF-8, 1009 for a message over the bound, 1002 for a fragment out
// of sequence.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import NIOCore
import NIOEmbedded
import NIOWebSocket
import Testing

@testable import FantasticWeb

// MARK: - the guard, behind the aggregator

/// The host's message layer on an embedded channel: the aggregator (bounded at
/// `bound`) then the guard; what reaches the tail is what an endpoint would see.
private func messageLayer(bound: Int) throws -> EmbeddedChannel {
    let ch = EmbeddedChannel()
    try ch.pipeline.syncOperations.addHandler(
        NIOWebSocketFrameAggregator(
            minNonFinalFragmentSize: 0, maxAccumulatedFrameCount: Int.max,
            maxAccumulatedFrameSize: bound))
    try ch.pipeline.syncOperations.addHandler(WebSocketMessageGuard())
    return ch
}

private func frame(
    _ ch: EmbeddedChannel, _ opcode: WebSocketOpcode, _ bytes: [UInt8], fin: Bool = true
) -> WebSocketFrame {
    WebSocketFrame(fin: fin, opcode: opcode, data: ch.allocator.buffer(bytes: bytes))
}

/// The close code the guard wrote, or nil when it wrote nothing.
private func closeCode(_ ch: EmbeddedChannel) throws -> WebSocketErrorCode? {
    ch.embeddedEventLoop.run()
    guard let out = try ch.readOutbound(as: WebSocketFrame.self) else { return nil }
    #expect(out.opcode == .connectionClose)
    var body = out.data
    #expect(body.readableBytes == 2, "python's close carries no reason")
    return body.readWebSocketErrorCode()
}

@Suite("web — websocket messages, not frames")
struct WebSocketMessageGuardTests {

    @Test func aTextMessageThatIsNotUTF8Closes1007() throws {
        for bad: [UInt8] in [
            [0x7B, 0xFF, 0xFE, 0x7D],  // not UTF-8 at all
            [0xC0, 0xAF],  // an overlong `/`
            [0xED, 0xA0, 0x80],  // a lone surrogate
            [0x61, 0xC3],  // a truncated sequence
        ] {
            let ch = try messageLayer(bound: MAX_FRAME)
            try ch.writeInbound(frame(ch, .text, bad))
            #expect(
                try ch.readInbound(as: WebSocketFrame.self) == nil, "\(bad) reached the endpoint")
            #expect(try closeCode(ch) == .dataInconsistentWithMessage, "\(bad)")
            #expect(ch.isActive == false, "\(bad)")
        }
    }

    @Test func aBinaryMessageIsNotDecoded() throws {
        let ch = try messageLayer(bound: MAX_FRAME)
        try ch.writeInbound(frame(ch, .binary, [0xFF, 0xFE]))
        #expect(try ch.readInbound(as: WebSocketFrame.self)?.opcode == .binary)
        #expect(try closeCode(ch) == nil)
    }

    /// A character split across two fragments is one valid message.
    @Test func fragmentsAreReassembledIntoOneMessage() throws {
        let ch = try messageLayer(bound: MAX_FRAME)
        try ch.writeInbound(frame(ch, .text, Array("{\"x\": \"".utf8) + [0xC3], fin: false))
        #expect(try ch.readInbound(as: WebSocketFrame.self) == nil, "half a message reached it")
        try ch.writeInbound(frame(ch, .continuation, [0xA9] + Array("\"}".utf8)))
        let whole = try #require(try ch.readInbound(as: WebSocketFrame.self))
        #expect(whole.opcode == .text && whole.fin)
        #expect(String(buffer: whole.unmaskedData) == "{\"x\": \"é\"}")
        #expect(try closeCode(ch) == nil)
    }

    @Test func aMessageOverTheBoundCloses1009() throws {
        let ch = try messageLayer(bound: 8)
        try ch.writeInbound(frame(ch, .text, Array("12345".utf8), fin: false))
        try ch.writeInbound(frame(ch, .continuation, Array("6789".utf8)))
        #expect(try ch.readInbound(as: WebSocketFrame.self) == nil)
        #expect(try closeCode(ch) == .messageTooLarge)
        #expect(ch.isActive == false)
    }

    @Test func aFragmentOutOfSequenceCloses1002() throws {
        // a continuation that continues nothing
        let orphan = try messageLayer(bound: MAX_FRAME)
        try orphan.writeInbound(frame(orphan, .continuation, Array("abc".utf8)))
        #expect(try closeCode(orphan) == .protocolError)
        // a new message while one is still open
        let interleaved = try messageLayer(bound: MAX_FRAME)
        try interleaved.writeInbound(frame(interleaved, .text, Array("{".utf8), fin: false))
        try interleaved.writeInbound(frame(interleaved, .text, Array("{}".utf8)))
        #expect(try interleaved.readInbound(as: WebSocketFrame.self) == nil)
        #expect(try closeCode(interleaved) == .protocolError)
    }

    /// One close per socket: what arrives after it is neither read nor refused.
    @Test func afterTheCloseNothingIsReadOrRefused() throws {
        let ch = try messageLayer(bound: MAX_FRAME)
        try ch.writeInbound(frame(ch, .text, [0xFF]))
        #expect(try closeCode(ch) == .dataInconsistentWithMessage)
        try? ch.writeInbound(frame(ch, .text, [0xFF]))
        try? ch.writeInbound(frame(ch, .text, Array("{}".utf8)))
        #expect(try ch.readInbound(as: WebSocketFrame.self) == nil)
        #expect(try closeCode(ch) == nil, "a second close was written")
    }
}

// MARK: - on the wire, through the host

/// A raw websocket client: the upgrade by hand, then frames with full control
/// over FIN, opcode and payload — what URLSession cannot send.
private final class RawWSClient {
    private let fd: Int32
    private var buf: [UInt8] = []

    init(port: UInt16, path: String) throws {
        fd = socket(AF_INET, Int32(SOCK_STREAM), 0)
        guard fd >= 0 else { throw RawHTTPError.socket }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        let ok = withUnsafePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard ok == 0 else { throw RawHTTPError.connect }
        var tv = timeval(tv_sec: 10, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        try write(
            Array(
                ("GET \(path) HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\nUpgrade: websocket\r\n"
                    + "Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                    + "Sec-WebSocket-Version: 13\r\n\r\n").utf8))
        while true {
            if let end = findHeadEnd() {
                let head = String(decoding: buf[0..<end], as: UTF8.self)
                buf.removeFirst(end + 4)
                guard head.hasPrefix("HTTP/1.1 101 ") else { throw RawHTTPError.connect }
                return
            }
            guard fill() else { throw RawHTTPError.connect }
        }
    }

    deinit { _ = Foundation.close(fd) }

    /// One masked client frame.
    func send(_ opcode: UInt8, _ payload: [UInt8], fin: Bool = true) throws {
        var f: [UInt8] = [(fin ? 0x80 : 0) | opcode]
        let n = payload.count
        if n < 126 {
            f.append(0x80 | UInt8(n))
        } else if n < 65536 {
            f += [0x80 | 126, UInt8(n >> 8), UInt8(n & 0xFF)]
        } else {
            f.append(0x80 | 127)
            for shift in stride(from: 56, through: 0, by: -8) {
                f.append(UInt8((n >> shift) & 0xFF))
            }
        }
        let mask: [UInt8] = [0x5A, 0x17, 0xC3, 0x08]
        f += mask
        var body = payload
        body.withUnsafeMutableBufferPointer { p in
            for i in 0..<p.count { p[i] ^= mask[i & 3] }
        }
        try write(f + body)
    }

    /// The next server frame `(opcode, payload)`; nil when the socket ended or
    /// went quiet.
    func next() -> (UInt8, [UInt8])? {
        guard let head = take(2) else { return nil }
        var n = Int(head[1] & 0x7F)
        if n == 126 {
            guard let b = take(2) else { return nil }
            n = Int(b[0]) << 8 | Int(b[1])
        } else if n == 127 {
            guard let b = take(8) else { return nil }
            n = b.reduce(0) { $0 << 8 | Int($1) }
        }
        guard let payload = take(n) else { return nil }
        return (head[0] & 0x0F, payload)
    }

    private func write(_ bytes: [UInt8]) throws {
        var sent = 0
        while sent < bytes.count {
            let n = bytes.withUnsafeBytes {
                Foundation.send(fd, $0.baseAddress!.advanced(by: sent), bytes.count - sent, 0)
            }
            guard n > 0 else { throw RawHTTPError.write }
            sent += n
        }
    }

    private func findHeadEnd() -> Int? {
        guard buf.count >= 4 else { return nil }
        for i in 0...(buf.count - 4)
        where buf[i] == 13 && buf[i + 1] == 10 && buf[i + 2] == 13 && buf[i + 3] == 10 {
            return i
        }
        return nil
    }

    private func fill() -> Bool {
        var chunk = [UInt8](repeating: 0, count: 65536)
        let r = recv(fd, &chunk, chunk.count, 0)
        guard r > 0 else { return false }
        buf += chunk[0..<r]
        return true
    }

    private func take(_ n: Int) -> [UInt8]? {
        while buf.count < n { guard fill() else { return nil } }
        let out = Array(buf[0..<n])
        buf.removeFirst(n)
        return out
    }
}

/// What a socket answered: the reply (or error) carrying `id`, the close code,
/// and every other text frame's `type`/`reason`.
private struct Answer {
    var answer: JSON?
    var close: UInt16?
    var others: [JSON] = []
}

/// Frames until the answer to `id`, the close, or silence.
private func answer(_ ws: RawWSClient, _ id: String) -> Answer {
    var out = Answer()
    while let (op, payload) = ws.next() {
        switch op {
        case 0x8:
            out.close = payload.count >= 2 ? UInt16(payload[0]) << 8 | UInt16(payload[1]) : nil
            return out
        case 0x1:
            let v = (try? JSON.parse(Data(payload))) ?? .null
            if v["id"].asString == id {
                out.answer = v
                return out
            }
            out.others.append(v)
        default:
            continue
        }
    }
    return out
}

private func call(_ id: String, pad: String = "") -> [UInt8] {
    Array(
        JSON.object([
            "type": "call", "id": .string(id), "target": "kernel_state",
            "payload": .object(["type": "reflect", "tree": "none", "pad": .string(pad)]),
        ]).serialize().utf8)
}

/// `web` booted with an open `web_ws` leg mounted (granted the root, the root
/// accepting it); the socket path `/kernel_state/ws`.
private func wsKernel() async throws -> Kernel {
    let k = try await startWebKernel()
    _ = await k.send([Item("web", ["type": "boot"])])
    let rec = await k.send([
        Item(
            "kernel",
            [
                "type": "create_agent", "parent_id": "web", "handler_module": "web_ws.tools",
                "id": "web_ws", "ingress_rule": "allow_all_ingress",
                "can_call": ["kernel_state": "*"],
            ])
    ])[0].result
    #expect(rec["id"] == "web_ws", "\(rec.serialize())")
    _ = await k.send([
        Item(
            "kernel",
            [
                "type": "update_agent", "id": "kernel_state",
                "callable_by": ["$self": "*", "$parent": "*", "$subtree": "*", "web_ws": "*"],
            ])
    ])
    wire(k, "web", "web_ws")
    _ = await k.send([Item("web", ["type": "mount", "child_id": "web_ws"])])
    return k
}

@Suite("web — websocket messages on the wire", .serialized)
struct WebSocketMessageWireTests {

    @Test func aCallInTwoFragmentsIsAnswered() async throws {
        let k = try await wsKernel()
        defer { Task { _ = await k.send([Item("web", ["type": "stop"])]) } }
        let ws = try RawWSClient(port: k.httpPort(), path: "/kernel_state/ws")
        let msg = call("frag1")
        try ws.send(0x1, Array(msg[0..<10]), fin: false)
        try ws.send(0x0, Array(msg[10...]))
        let got = answer(ws, "frag1")
        #expect(got.answer?["type"] == "reply", "\(got)")
    }

    /// A complete call in a NON-final frame is not a message: joined with what
    /// follows it, it is not JSON — the frame error, nothing dispatched.
    @Test func aCompleteCallInANonFinalFrameIsNotDispatched() async throws {
        let k = try await wsKernel()
        defer { Task { _ = await k.send([Item("web", ["type": "stop"])]) } }
        let ws = try RawWSClient(port: k.httpPort(), path: "/kernel_state/ws")
        try ws.send(0x1, call("smuggled"), fin: false)
        try ws.send(0x0, Array(" trailing-garbage".utf8))
        try ws.send(0x1, call("after"))
        let got = answer(ws, "after")
        #expect(got.answer?["type"] == "reply", "the socket is kept: \(got)")
        #expect(!got.others.contains { $0["id"] == "smuggled" }, "\(got)")
        #expect(got.others.contains { $0["type"] == "error" }, "the frame error: \(got)")
    }

    @Test func aTextMessageThatIsNotUTF8Closes1007() async throws {
        let k = try await wsKernel()
        defer { Task { _ = await k.send([Item("web", ["type": "stop"])]) } }
        let ws = try RawWSClient(port: k.httpPort(), path: "/kernel_state/ws")
        var msg = call("u1")
        msg.insert(contentsOf: [0xFF, 0xFE], at: msg.count - 3)
        try ws.send(0x1, msg)
        let got = answer(ws, "u1")
        #expect(got.answer == nil, "\(got)")
        #expect(got.close == 1007, "\(got)")
    }

    /// The bound is the ONE FRAME CAP: one byte over it, in fragments each well
    /// under the frame size, closes 1009 — nothing dispatched.
    @Test func aMessageOverTheFrameCapCloses1009() async throws {
        let k = try await wsKernel()
        defer { Task { _ = await k.send([Item("web", ["type": "stop"])]) } }
        let ws = try RawWSClient(port: k.httpPort(), path: "/kernel_state/ws")
        let base = call("big").count
        let msg = call("big", pad: String(repeating: "x", count: MAX_FRAME + 1 - base))
        #expect(msg.count == MAX_FRAME + 1)
        let half = msg.count / 2
        try ws.send(0x1, Array(msg[0..<half]), fin: false)
        try ws.send(0x0, Array(msg[half...]))
        let got = answer(ws, "big")
        #expect(got.answer == nil, "\(got.close as Any) \(got.others)")
        #expect(got.close == 1009, "\(got.close as Any) \(got.others)")
    }
}
