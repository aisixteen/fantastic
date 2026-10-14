// The websocket MESSAGE layer every upgraded route is served behind.
//
// NIO's server upgrader hands a route's endpoint raw FRAMES. Python's websockets
// server (and rust's tungstenite) reassemble a fragmented message, bound it, and
// refuse text that is not UTF-8. The host does the same in front of every
// endpoint: `NIOWebSocketFrameAggregator` reassembles fragments into ONE
// message of at most io_bridge's `MAX_FRAME` bytes (the swift client's bound),
// and this guard closes the socket with python's code and python's empty reason
// — 1007 for a text message that is not UTF-8, 1009 for a message over the
// bound, 1002 for a fragment out of sequence.

import NIOCore
import NIOWebSocket

/// Sits between `NIOWebSocketFrameAggregator` and the route's endpoint: only
/// whole, valid messages reach the endpoint.
final class WebSocketMessageGuard: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = WebSocketFrame
    typealias InboundOut = WebSocketFrame
    typealias OutboundOut = WebSocketFrame

    /// The close is sent: nothing more is read or refused on this socket.
    private var closing = false

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        if closing { return }
        let frame = unwrapInboundIn(data)
        // STRICT UTF-8, as python decodes a text message: an overlong form, a
        // surrogate or a truncated sequence is an error, never U+FFFD.
        if frame.opcode == .text,
            transcode(
                frame.unmaskedData.readableBytesView.makeIterator(), from: UTF8.self,
                to: UTF32.self, stoppingOnError: true, into: { _ in })
        {
            refuse(context: context, .dataInconsistentWithMessage)
            return
        }
        context.fireChannelRead(data)
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        guard let error = error as? NIOWebSocketFrameAggregator.Error else {
            context.fireErrorCaught(error)
            return
        }
        if closing { return }
        refuse(
            context: context,
            error == .accumulatedFrameSizeIsTooLarge ? .messageTooLarge : .protocolError)
    }

    private func refuse(context: ChannelHandlerContext, _ code: WebSocketErrorCode) {
        closing = true
        var data = context.channel.allocator.buffer(capacity: 2)
        data.write(webSocketErrorCode: code)
        let close = WebSocketFrame(fin: true, opcode: .connectionClose, data: data)
        context.writeAndFlush(wrapOutboundOut(close)).whenComplete { _ in
            context.close(promise: nil)
        }
    }
}
