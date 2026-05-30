//! WebSocket client transport — `tokio-tungstenite` against a remote
//! `fantastic-web` `/<peer_id>/ws` surface.
//!
//! Frames serialize as JSON text. The wire shape matches what
//! `fantastic-web`'s WS server expects (`{type:"call", target,
//! payload, id}` and `{type:"reply", id, data}` / `{type:"error", id,
//! error}`) so the same code path round-trips against any remote
//! kernel hosting a web agent.
//!
//! The connect helper does NOT spawn the read loop — the bridge
//! state owns that, since it needs the inbound frames routed to its
//! `kernel.send` dispatcher + pending oneshot map.

use super::{BridgeTransport, TransportError};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// WebSocket client transport.
///
/// On construction we split the socket and spawn a reader task that
/// pushes inbound text frames onto an mpsc queue. `recv_frame`
/// reads from that queue; `send_frame` writes directly to the
/// sink (guarded by a Mutex so concurrent sends serialize cleanly).
pub struct WsTransport {
    sink: Mutex<futures_util::stream::SplitSink<WsStream, Message>>,
    rx: Mutex<mpsc::Receiver<Result<Value, TransportError>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
}

impl WsTransport {
    /// Connect to `url` (e.g. `ws://host:port/peer_id/ws`) with a
    /// default 5s timeout and return a transport wrapping the live
    /// socket. Spawns the reader task as a side-effect.
    pub async fn connect(url: &str) -> Result<Arc<Self>, TransportError> {
        Self::connect_with_timeout(url, std::time::Duration::from_secs(5)).await
    }

    /// Connect with an explicit timeout. Used by tests + callers who
    /// want a tighter bound than the 5s default.
    pub async fn connect_with_timeout(
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<Arc<Self>, TransportError> {
        let connect_fut = connect_async(url);
        let (stream, _resp) = match tokio::time::timeout(timeout, connect_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(TransportError::Other(format!("ws connect {url}: {e}"))),
            Err(_) => {
                return Err(TransportError::Other(format!(
                    "ws connect {url}: timeout after {:?}",
                    timeout
                )))
            }
        };
        let (sink, mut source) = stream.split();
        let (tx, rx) = mpsc::channel::<Result<Value, TransportError>>(64);
        let reader = tokio::spawn(async move {
            while let Some(msg) = source.next().await {
                let frame_result = match msg {
                    Ok(Message::Text(t)) => serde_json::from_str::<Value>(&t)
                        .map_err(|e| TransportError::Other(format!("ws decode: {e}"))),
                    Ok(Message::Binary(b)) => serde_json::from_slice::<Value>(&b)
                        .map_err(|e| TransportError::Other(format!("ws decode: {e}"))),
                    Ok(Message::Close(_)) => {
                        let _ = tx
                            .send(Err(TransportError::ConnectionClosed(
                                "peer sent close frame".into(),
                            )))
                            .await;
                        break;
                    }
                    Ok(_) => continue, // ping/pong/etc handled by tungstenite
                    Err(e) => Err(TransportError::ConnectionClosed(format!("ws read: {e}"))),
                };
                let is_err = matches!(&frame_result, Err(TransportError::ConnectionClosed(_)));
                if tx.send(frame_result).await.is_err() {
                    break;
                }
                if is_err {
                    break;
                }
            }
        });
        Ok(Arc::new(Self {
            sink: Mutex::new(sink),
            rx: Mutex::new(rx),
            reader_task: Mutex::new(Some(reader)),
        }))
    }
}

#[async_trait]
impl BridgeTransport for WsTransport {
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError> {
        let text = serde_json::to_string(&frame)
            .map_err(|e| TransportError::Other(format!("ws encode: {e}")))?;
        let mut sink = self.sink.lock().await;
        sink.send(Message::Text(text))
            .await
            .map_err(|e| TransportError::ConnectionClosed(format!("ws send: {e}")))
    }

    async fn recv_frame(&self) -> Result<Value, TransportError> {
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(r) => r,
            None => Err(TransportError::ConnectionClosed(
                "ws reader task ended".into(),
            )),
        }
    }

    async fn close(&self) {
        // Tear down sink first so any in-flight send fails fast.
        {
            let mut sink = self.sink.lock().await;
            let _ = sink.close().await;
        }
        if let Some(handle) = self.reader_task.lock().await.take() {
            handle.abort();
        }
    }
}
