//! permessage-deflate (RFC 7692) on the WS legs — negotiated and applied as
//! python's `websockets` does under uvicorn (the `web_ws` server) and in
//! `ws_bridge` (the client).
//!
//! Python gets the extension for free: uvicorn's server answers an offer with
//! `ServerPerMessageDeflateFactory()`, and `websockets.connect` offers
//! `permessage-deflate; client_max_window_bits`. `tungstenite` has no extension
//! support (it refuses any frame with RSV1 set), so it runs here over a
//! [`DeflateStream`]: the byte stream under the WebSocket, inflating what comes
//! in and deflating what goes out, frame by frame, with the parameters the
//! handshake settled ([`server_negotiate`] / [`client_accept`]).

mod codec;
mod negotiate;
mod stream;

pub use codec::{PerMessageDeflate, MAX_SIZE};
pub use negotiate::{
    build_extension, client_accept, parse_extension, server_negotiate, ExtHeader, ExtParam, Params,
    Refused, CLIENT_OFFER, NAME,
};
pub use stream::DeflateStream;

#[cfg(test)]
mod tests;
