// WS CLIENT CLOSE HANDSHAKE — RFC 6455 §5.5.1, pinned.
//
// A peer's Close MUST be answered with a Close carrying the PEER's status code,
// once, and nothing may follow it on the wire. `websockets` (python) decides
// `ConnectionClosedOK` vs `ConnectionClosedError` on
// `close_rcvd.code in OK_CLOSE_CODES and close_sent is not None`, so a silent
// TCP drop turns a peer's orderly 1001 into an ERROR on python's side and a
// clean close on swift's. The behaviour shipped unpinned; this is the twin.

import NIOCore
import NIOEmbedded
import NIOHTTP1
import NIOWebSocket
import Testing

@testable import FantasticKernelBridge

@Suite("WS client: the closing handshake")
struct WebSocketClientCloseTests {
    private func closeFrame(_ ch: EmbeddedChannel, _ code: WebSocketErrorCode) -> WebSocketFrame {
        var payload = ch.allocator.buffer(capacity: 2)
        payload.write(webSocketErrorCode: code)
        return WebSocketFrame(fin: true, opcode: .connectionClose, data: payload)
    }

    @Test func aPeerCloseIsEchoedWithThePeersCodeAndNothingFollows() throws {
        let inbox = WSClientInbox()
        let ch = EmbeddedChannel(handler: WSClientHandler(inbox: inbox))
        // The peer goes away (1001) — the code python's `websockets` counts as
        // an OK close ONLY when the handshake completes.
        try ch.writeInbound(closeFrame(ch, .goingAway))
        ch.embeddedEventLoop.run()
        let echo = try #require(try ch.readOutbound(as: WebSocketFrame.self))
        #expect(echo.opcode == .connectionClose)
        #expect(echo.maskKey != nil, "client→server frames are masked (RFC 6455 §5.3)")
        // `readOutbound` hands back the frame as WRITTEN — the encoder (which is
        // what applies the mask) is not in this pipeline, so `data` is the
        // payload verbatim.
        var body = echo.data
        #expect(body.readWebSocketErrorCode() == .goingAway, "the PEER's code, not one of ours")
        let after = try ch.readOutbound(as: WebSocketFrame.self)
        #expect(after == nil, "nothing is written after the close echo")
        #expect(ch.isActive == false, "the channel drops once the echo is flushed")
    }

    @Test func aSecondCloseIsNeverEchoedTwice() throws {
        let inbox = WSClientInbox()
        let ch = EmbeddedChannel(handler: WSClientHandler(inbox: inbox))
        try ch.writeInbound(closeFrame(ch, .normalClosure))
        ch.embeddedEventLoop.run()
        _ = try ch.readOutbound(as: WebSocketFrame.self)
        try? ch.writeInbound(closeFrame(ch, .normalClosure))
        ch.embeddedEventLoop.run()
        #expect(try ch.readOutbound(as: WebSocketFrame.self) == nil, "one echo per connection")
    }

    /// The close is a CLEAN end for a reader: `receive()` throws `Closed`, the
    /// shape every bridge leg treats as "the peer hung up", never an abort with
    /// a status code baked into an error message.
    @Test func aReaderSeesACleanClosedNotAnError() async throws {
        let inbox = WSClientInbox()
        let ch = EmbeddedChannel(handler: WSClientHandler(inbox: inbox))
        try ch.writeInbound(closeFrame(ch, .goingAway))
        ch.embeddedEventLoop.run()
        do {
            _ = try await inbox.next()
            Issue.record("a closed socket must not deliver a message")
        } catch let e as NIOWebSocketClient.Closed {
            #expect(e.reason == "websocket: peer closed")
        }
    }
}

/// The upgrade REQUEST the kernel itself writes: python `websockets` sends
/// `Host: <host>:<port>` for a non-default port (`build_host`) and no body
/// framing header on the bodiless GET.
@Suite("WS client: the upgrade request")
struct WebSocketClientUpgradeRequestTests {
    @Test func theHostCarriesANonDefaultPortAndNoBodyFraming() throws {
        let host = NIOWebSocketClient.hostHeader
        #expect(host("127.0.0.1", 57967, false) == "127.0.0.1:57967")
        #expect(host("example.com", 80, false) == "example.com")
        #expect(host("example.com", 443, true) == "example.com")
        #expect(host("::1", 9, false) == "[::1]:9")

        let ch = EmbeddedChannel()
        let signal = WSUpgradeSignal(ch.eventLoop.makePromise(of: Void.self))
        defer { signal.succeed() }
        try ch.pipeline.syncOperations.addHandler(
            WSUpgradeRequester(
                host: "127.0.0.1:57967", uri: "/x", subprotocols: [], authToken: nil,
                signal: signal))
        ch.pipeline.fireChannelActive()
        let part = try #require(try ch.readOutbound(as: HTTPClientRequestPart.self))
        guard case .head(let head) = part else {
            Issue.record("the first part written is not the request head")
            return
        }
        #expect(head.headers["Host"] == ["127.0.0.1:57967"], "\(head.headers)")
        #expect(head.headers["Content-Length"].isEmpty, "\(head.headers)")
    }
}

/// python `websockets.connect`'s keepalive: a ping every 20 s; a pong keeps the
/// connection, and a missing one fails it 1011 `keepalive ping timeout` — the
/// reader sees the close (a bridge's `bridge_down`) without any send.
@Suite("WS client: keepalive")
struct WebSocketClientKeepaliveTests {
    @Test func aMissingPongFailsTheConnection1011() throws {
        let inbox = WSClientInbox()
        let ch = EmbeddedChannel(handler: WSClientHandler(inbox: inbox))
        let loop = ch.embeddedEventLoop
        loop.advanceTime(by: .seconds(WSClientHandler.pingIntervalSeconds))
        let ping = try #require(try ch.readOutbound(as: WebSocketFrame.self))
        #expect(ping.opcode == .ping && ping.maskKey != nil)
        // a pong answers it — the next tick pings again instead of failing
        try ch.writeInbound(
            WebSocketFrame(fin: true, opcode: .pong, data: ch.allocator.buffer(capacity: 0)))
        loop.advanceTime(by: .seconds(WSClientHandler.pongTimeoutSeconds))
        let again = try #require(try ch.readOutbound(as: WebSocketFrame.self))
        #expect(again.opcode == .ping)
        // no pong this time: the connection fails 1011
        loop.advanceTime(by: .seconds(WSClientHandler.pongTimeoutSeconds))
        let close = try #require(try ch.readOutbound(as: WebSocketFrame.self))
        #expect(close.opcode == .connectionClose)
        var body = close.data
        #expect(body.readWebSocketErrorCode() == .unexpectedServerError)
        #expect(body.readString(length: body.readableBytes) == "keepalive ping timeout")
        loop.run()
        #expect(!ch.isActive, "the connection was not closed")
    }
}
