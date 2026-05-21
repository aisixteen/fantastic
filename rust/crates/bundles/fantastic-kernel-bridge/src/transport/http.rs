//! HTTP request/reply transport — `reqwest` POSTs to a remote
//! `web_rest` surface.
//!
//! Asymmetric on purpose: there is no server→client push over plain
//! HTTP, so `recv_frame` reads off a self-populated queue that
//! `send_frame` fills synchronously with each round-trip's response.
//! From the bridge's read-loop perspective the shape is the same as
//! WS — one frame in, one frame out — but the HTTP path has no
//! inbound `call` traffic and no `bridge_down` until the bridge
//! itself closes.
//!
//! Wire mapping: the bridge sends frames in the standard `{type:"call",
//! target:peer_id, payload:{type:"forward", target, payload,
//! corr_id}, id:corr_id}` shape. This transport unwraps the
//! `forward` envelope and POSTs the INNER payload to
//! `{base_url}{inner_target}`. The JSON response is enqueued as a
//! `{type:"reply", id:corr_id, data:<response>}` frame.

use super::{BridgeTransport, TransportError};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// HTTP transport. Owns a `reqwest::Client` + an in-process queue
/// that `recv_frame` reads from.
pub struct HttpTransport {
    base: String,
    client: reqwest::Client,
    rx: Mutex<mpsc::Receiver<Value>>,
    tx: mpsc::Sender<Value>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl HttpTransport {
    /// Build a transport rooted at `base_url`. Trailing slash is
    /// added if missing so `{base}{target}` composes cleanly.
    pub fn new(base_url: &str) -> Arc<Self> {
        let base = if base_url.ends_with('/') {
            base_url.to_string()
        } else {
            format!("{base_url}/")
        };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let (tx, rx) = mpsc::channel::<Value>(64);
        Arc::new(Self {
            base,
            client,
            rx: Mutex::new(rx),
            tx,
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
}

#[async_trait]
impl BridgeTransport for HttpTransport {
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError> {
        use std::sync::atomic::Ordering;
        if self.closed.load(Ordering::SeqCst) {
            return Err(TransportError::ConnectionClosed(
                "HttpTransport closed".into(),
            ));
        }
        let ty = frame.get("type").and_then(Value::as_str).unwrap_or("");
        if ty != "call" {
            return Err(TransportError::Other(format!(
                "HttpTransport supports only 'call' frames (got {ty:?})"
            )));
        }
        // Unwrap the forward envelope — HTTP has no peer bridge, just
        // the remote rest surface directly. Inner target + payload
        // become the POST URL + body.
        let payload = frame.get("payload").cloned().unwrap_or(Value::Null);
        let (target, body) =
            if payload.get("type").and_then(Value::as_str).unwrap_or("") == "forward" {
                (
                    payload
                        .get("target")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    payload.get("payload").cloned().unwrap_or(Value::Null),
                )
            } else {
                (
                    frame
                        .get("target")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    payload,
                )
            };
        if target.is_empty() {
            return Err(TransportError::Other("HttpTransport: empty target".into()));
        }
        let url = format!("{}{}", self.base, target);
        let corr_id = frame.get("id").cloned().unwrap_or(Value::Null);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| TransportError::ConnectionClosed(format!("http POST {url}: {e}")))?;
        let status = resp.status();
        let data: Value = if status.is_success() {
            match resp.json::<Value>().await {
                Ok(v) => v,
                Err(_) => Value::Null,
            }
        } else {
            let text = resp.text().await.unwrap_or_default();
            let truncated: String = text.chars().take(200).collect();
            json!({"error": format!("HTTP {}: {}", status.as_u16(), truncated)})
        };
        let reply = json!({
            "type": "reply",
            "id": corr_id,
            "data": data,
        });
        self.tx
            .send(reply)
            .await
            .map_err(|_| TransportError::ConnectionClosed("HttpTransport queue closed".into()))?;
        Ok(())
    }

    async fn recv_frame(&self) -> Result<Value, TransportError> {
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(v) => Ok(v),
            None => Err(TransportError::ConnectionClosed(
                "HttpTransport queue dropped".into(),
            )),
        }
    }

    async fn close(&self) {
        use std::sync::atomic::Ordering;
        self.closed.store(true, Ordering::SeqCst);
        // Close the queue so any pending `recv_frame` resolves.
        // (We do this by dropping the sender — but `self.tx` is owned;
        // we can't move out of `&self`. Instead, future sends will
        // short-circuit on the closed flag, and recv will wake when
        // the last `Sender` clone is dropped at struct drop.)
    }
}
