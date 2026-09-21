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
