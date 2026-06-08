//! cloud_bridge transport — dial-OUT relay leg with end-to-end TLS 1.3 mTLS.
//!
//! Mirror of the Python `cloud_bridge`. Both peers dial OUT (WSS) to a relay that
//! pairs them by `(tenant, rendezvous)` and forwards OPAQUE binary frames; the two
//! peers run a mutually-authenticated TLS 1.3 handshake (rustls, driven over the
//! relay's byte pipe — no socket) with self-signed Ed25519 device certs, pinned by
//! **public key** (extracted from `approved_peer_certs`), and tunnel the SAME
//! kernel-bridge `call`/`reply`/`event` JSON frames as TLS application data,
//! length-delimited (`u32` big-endian prefix). The relay sees only ciphertext.
//!
//! The TLS + framing core is generic over a [`ByteChannel`] (the opaque-binary
//! layer): [`WsByteChannel`] in production, [`MemoryByteChannel`] for the in-process
//! loopback unit test.

use super::{BridgeTransport, TransportError};
use async_trait::async_trait;
use std::io::{Cursor, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConnection, Connection, DigitallySignedStruct, ServerConnection, SignatureScheme,
};

pub(crate) const SUBPROTOCOL: &str = "fantastic.relay.v1";
const KEEPALIVE_TYPE: &str = "keepalive";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FRAME: usize = 16 * 1024 * 1024;
const HDR: usize = 4;

// ── opaque-binary channel (the WS Binary layer) ─────────────────────

/// The opaque-binary-frame layer the TLS engine rides on. Each `send_bytes` /
/// `recv_bytes` is one WS Binary frame (the relay forwards them verbatim).
#[async_trait]
pub(crate) trait ByteChannel: Send + Sync {
    async fn send_bytes(&self, b: Vec<u8>) -> Result<(), TransportError>;
    async fn recv_bytes(&self) -> Result<Vec<u8>, TransportError>;
    async fn close(&self);
}

/// Production channel: a `tokio-tungstenite` client to the relay, Binary frames.
pub(crate) struct WsByteChannel {
    sink: Mutex<futures_util::stream::SplitSink<WsStream, Message>>,
    rx: Mutex<mpsc::Receiver<Result<Vec<u8>, TransportError>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
}
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

impl WsByteChannel {
    /// Dial `relay_url` offering `fantastic.relay.v1, <token>` as the subprotocol
    /// (the relay authenticates + pairs on it). Token passed verbatim.
    pub(crate) async fn connect(relay_url: &str, token: &str) -> Result<Arc<Self>, TransportError> {
        let mut req = relay_url
            .into_client_request()
            .map_err(|e| TransportError::Other(format!("cloud_bridge: bad relay url: {e}")))?;
        req.headers_mut().append(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(&format!("{SUBPROTOCOL}, {token}")).map_err(|e| {
                TransportError::Other(format!("cloud_bridge: bad token header: {e}"))
            })?,
        );
        let connect_fut = connect_async(req);
        let (stream, _resp) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, connect_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                return Err(TransportError::Other(format!(
                    "cloud_bridge relay dial: {e}"
                )))
            }
            Err(_) => {
                return Err(TransportError::Other(
                    "cloud_bridge relay dial: timeout".into(),
                ))
            }
        };
        let (sink, mut source) = stream.split();
        let (tx, rx) = mpsc::channel::<Result<Vec<u8>, TransportError>>(64);
        let reader = tokio::spawn(async move {
            while let Some(msg) = source.next().await {
                let out = match msg {
                    Ok(Message::Binary(b)) => Ok(b.to_vec()),
                    Ok(Message::Text(t)) => Ok(t.as_bytes().to_vec()),
                    Ok(Message::Close(_)) => {
                        let _ = tx
                            .send(Err(TransportError::ConnectionClosed("relay closed".into())))
                            .await;
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => Err(TransportError::ConnectionClosed(format!("relay read: {e}"))),
                };
                let is_err = matches!(&out, Err(TransportError::ConnectionClosed(_)));
                if tx.send(out).await.is_err() || is_err {
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
impl ByteChannel for WsByteChannel {
    async fn send_bytes(&self, b: Vec<u8>) -> Result<(), TransportError> {
        let mut sink = self.sink.lock().await;
        sink.send(Message::Binary(b))
            .await
            .map_err(|e| TransportError::ConnectionClosed(format!("relay send: {e}")))
    }
    async fn recv_bytes(&self) -> Result<Vec<u8>, TransportError> {
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(r) => r,
            None => Err(TransportError::ConnectionClosed(
                "relay reader ended".into(),
            )),
        }
    }
    async fn close(&self) {
        {
            let mut sink = self.sink.lock().await;
            let _ = sink.close().await;
        }
        if let Some(h) = self.reader_task.lock().await.take() {
            h.abort();
        }
    }
}

/// In-process channel for the loopback unit test — a cross-wired mpsc pair.
#[cfg(test)]
pub(crate) struct MemoryByteChannel {
    tx: mpsc::Sender<Vec<u8>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
}
#[cfg(test)]
impl MemoryByteChannel {
    pub(crate) fn pair() -> (Arc<Self>, Arc<Self>) {
        let (tx_ab, rx_ab) = mpsc::channel(64);
        let (tx_ba, rx_ba) = mpsc::channel(64);
        let a = Arc::new(Self {
            tx: tx_ab,
            rx: Mutex::new(rx_ba),
        });
        let b = Arc::new(Self {
            tx: tx_ba,
            rx: Mutex::new(rx_ab),
        });
        (a, b)
    }
}
#[cfg(test)]
#[async_trait]
impl ByteChannel for MemoryByteChannel {
    async fn send_bytes(&self, b: Vec<u8>) -> Result<(), TransportError> {
        self.tx
            .send(b)
            .await
            .map_err(|_| TransportError::ConnectionClosed("memory channel closed".into()))
    }
    async fn recv_bytes(&self) -> Result<Vec<u8>, TransportError> {
        let mut rx = self.rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| TransportError::ConnectionClosed("memory channel closed".into()))
    }
    async fn close(&self) {}
}

// ── Ed25519 self-signed cert (deterministic, from the device id_key) ─

/// PKCS8 DER wrapping a raw 32-byte Ed25519 seed (RFC 8410 v1 prefix).
fn ed25519_pkcs8(seed32: &[u8]) -> Vec<u8> {
    let mut der = Vec::with_capacity(16 + 32);
    der.extend_from_slice(&[
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ]);
    der.extend_from_slice(seed32);
    der
}

/// A deterministic self-signed Ed25519 cert for a device identity key, plus the
/// key. Deterministic (fixed serial/validity + Ed25519's deterministic signature)
/// so it is stable to pin across reboots. Returns (cert_der, key_pkcs8_der).
pub(crate) fn self_signed_cert(id_key: &[u8]) -> Result<(Vec<u8>, Vec<u8>), TransportError> {
    use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
    use rustls::pki_types::PrivatePkcs8KeyDer;
    let pkcs8 = ed25519_pkcs8(id_key);
    let key = KeyPair::from_pkcs8_der_and_sign_algo(
        &PrivatePkcs8KeyDer::from(pkcs8.clone()),
        &rcgen::PKCS_ED25519,
    )
    .map_err(|e| TransportError::Other(format!("cloud_bridge cert key: {e}")))?;
    let cn = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.public_key_raw());
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| TransportError::Other(format!("cloud_bridge cert params: {e}")))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    params.distinguished_name = dn;
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2050, 1, 1);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let cert = params
        .self_signed(&key)
        .map_err(|e| TransportError::Other(format!("cloud_bridge self-sign: {e}")))?;
    Ok((cert.der().to_vec(), pkcs8))
}

/// Wrap a cert DER as standard PEM (so a peer can pin it as `approved_peer_certs`,
/// and the harness can collect a rust leg's cert).
pub(crate) fn der_to_pem(der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut s = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        s.push('\n');
    }
    s.push_str("-----END CERTIFICATE-----\n");
    s
}

/// The raw Ed25519 public key embedded in a cert (its durable `peer_id`).
fn cert_pubkey(der: &[u8]) -> Result<Vec<u8>, TransportError> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| TransportError::Other(format!("cloud_bridge parse peer cert: {e}")))?;
    Ok(cert.public_key().subject_public_key.data.to_vec())
}

/// Approved peer pubkeys, extracted from the pinned `approved_peer_certs` (PEM).
fn approved_pubkeys(pems: &[String]) -> Result<Vec<Vec<u8>>, TransportError> {
    let mut out = Vec::new();
    for pem in pems {
        let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
            .map_err(|e| TransportError::Other(format!("cloud_bridge approved cert pem: {e}")))?;
        out.push(cert_pubkey(&p.contents)?);
    }
    Ok(out)
}

// ── pinning verifiers (check peer leaf cert pubkey ∈ approved set) ───

#[derive(Debug)]
struct Pinned {
    approved: Vec<Vec<u8>>,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}
impl Pinned {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let pk = cert_pubkey(end_entity.as_ref())
            .map_err(|e| rustls::Error::General(format!("peer cert: {e}")))?;
        if self.approved.iter().any(|a| a == &pk) {
            Ok(())
        } else {
            Err(rustls::Error::General(
                "cloud_bridge: peer cert not in approved device list".into(),
            ))
        }
    }
}

#[derive(Debug)]
struct PinnedServerVerifier(Arc<Pinned>);
impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.0.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.algs)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.algs.supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedClientVerifier(Arc<Pinned>);
impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.0.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.algs)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.algs.supported_schemes()
    }
}

// ── the transport ───────────────────────────────────────────────────

/// A `cloud_bridge` leg: a [`ByteChannel`] to the relay + an mTLS session over it.
pub(crate) struct CloudTransport {
    channel: Arc<dyn ByteChannel>,
    conn: Mutex<Connection>,
    rbuf: Mutex<Vec<u8>>,
    /// The verified peer's Ed25519 public key (its durable identity). Read by the
    /// loopback test today; surfaced via reflect once rust reflect mirrors it.
    #[allow(dead_code)]
    pub(crate) peer_pubkey: Vec<u8>,
}

impl CloudTransport {
    /// Build the mTLS session over `channel` and run the handshake (pinning the
    /// peer to `approved_certs_pem`). `server` selects the TLS role.
    pub(crate) async fn connect(
        channel: Arc<dyn ByteChannel>,
        server: bool,
        cert_der: Vec<u8>,
        key_pkcs8: Vec<u8>,
        approved_certs_pem: &[String],
    ) -> Result<Arc<Self>, TransportError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let pinned = Arc::new(Pinned {
            approved: approved_pubkeys(approved_certs_pem)?,
            algs: provider.signature_verification_algorithms,
        });
        let certs = vec![CertificateDer::from(cert_der)];
        let key = PrivateKeyDer::try_from(key_pkcs8)
            .map_err(|e| TransportError::Other(format!("cloud_bridge key der: {e}")))?;

        let mut conn = if server {
            let cfg = rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| TransportError::Other(format!("cloud_bridge server cfg: {e}")))?
                .with_client_cert_verifier(Arc::new(PinnedClientVerifier(pinned)))
                .with_single_cert(certs, key)
                .map_err(|e| TransportError::Other(format!("cloud_bridge server cert: {e}")))?;
            Connection::Server(
                ServerConnection::new(Arc::new(cfg))
                    .map_err(|e| TransportError::Other(format!("cloud_bridge server conn: {e}")))?,
            )
        } else {
            let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| TransportError::Other(format!("cloud_bridge client cfg: {e}")))?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PinnedServerVerifier(pinned)))
                .with_client_auth_cert(certs, key)
                .map_err(|e| TransportError::Other(format!("cloud_bridge client cert: {e}")))?;
            // ServerName is unused (we pin by pubkey, hostname verification off).
            let name = ServerName::try_from("relay.invalid")
                .map_err(|e| TransportError::Other(format!("cloud_bridge name: {e}")))?;
            Connection::Client(
                ClientConnection::new(Arc::new(cfg), name)
                    .map_err(|e| TransportError::Other(format!("cloud_bridge client conn: {e}")))?,
            )
        };

        // Let a single large frame be written into the plaintext buffer in one
        // go (we drain to TLS records right after); default is bounded.
        conn.set_buffer_limit(None);
        drive_handshake(&mut conn, &*channel).await?;
        let peer_pubkey = conn
            .peer_certificates()
            .and_then(|c| c.first())
            .ok_or_else(|| TransportError::Other("cloud_bridge: no peer cert".into()))
            .and_then(|c| cert_pubkey(c.as_ref()))?;

        Ok(Arc::new(Self {
            channel,
            conn: Mutex::new(conn),
            rbuf: Mutex::new(Vec::new()),
            peer_pubkey,
        }))
    }
}

/// Pump a TLS handshake to completion over the byte channel (mirror of Python's
/// `_drive`): flush outbound records, feed inbound records.
async fn drive_handshake(
    conn: &mut Connection,
    ch: &dyn ByteChannel,
) -> Result<(), TransportError> {
    loop {
        let mut out = Vec::new();
        while conn.wants_write() {
            conn.write_tls(&mut out)
                .map_err(|e| TransportError::Other(format!("cloud_bridge write_tls: {e}")))?;
        }
        if !out.is_empty() {
            ch.send_bytes(out).await?;
        }
        if !conn.is_handshaking() {
            return Ok(());
        }
        let data = ch.recv_bytes().await?;
        let mut cur = Cursor::new(data.as_slice());
        while (cur.position() as usize) < data.len() {
            if conn
                .read_tls(&mut cur)
                .map_err(|e| TransportError::Other(format!("cloud_bridge read_tls: {e}")))?
                == 0
            {
                break;
            }
            conn.process_new_packets().map_err(|e| {
                TransportError::ConnectionClosed(format!("cloud_bridge handshake: {e}"))
            })?;
        }
    }
}

#[async_trait]
impl BridgeTransport for CloudTransport {
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError> {
        let json = serde_json::to_vec(&frame)
            .map_err(|e| TransportError::Other(format!("cloud_bridge encode: {e}")))?;
        let mut payload = Vec::with_capacity(HDR + json.len());
        payload.extend_from_slice(&(json.len() as u32).to_be_bytes());
        payload.extend_from_slice(&json);
        let out = {
            let mut conn = self.conn.lock().await;
            conn.writer().write_all(&payload).map_err(|e| {
                TransportError::ConnectionClosed(format!("cloud_bridge plain: {e}"))
            })?;
            let mut out = Vec::new();
            while conn.wants_write() {
                conn.write_tls(&mut out).map_err(|e| {
                    TransportError::ConnectionClosed(format!("cloud_bridge tls: {e}"))
                })?;
            }
            out
        };
        self.channel.send_bytes(out).await
    }

    async fn recv_frame(&self) -> Result<Value, TransportError> {
        loop {
            // Try to pop a complete length-delimited frame from the buffer.
            {
                let mut rbuf = self.rbuf.lock().await;
                if rbuf.len() >= HDR {
                    let n = u32::from_be_bytes([rbuf[0], rbuf[1], rbuf[2], rbuf[3]]) as usize;
                    if n > MAX_FRAME {
                        return Err(TransportError::ConnectionClosed(
                            "cloud_bridge: frame exceeds cap".into(),
                        ));
                    }
                    if rbuf.len() >= HDR + n {
                        let body = rbuf[HDR..HDR + n].to_vec();
                        rbuf.drain(..HDR + n);
                        drop(rbuf);
                        let frame: Value = serde_json::from_slice(&body).map_err(|e| {
                            TransportError::Other(format!("cloud_bridge decode: {e}"))
                        })?;
                        if frame.get("type").and_then(Value::as_str) == Some(KEEPALIVE_TYPE) {
                            continue; // heartbeat — never surfaced
                        }
                        return Ok(frame);
                    }
                }
            }
            // Need more plaintext — read this inbound batch, interleaving
            // read_tls + process so rustls's bounded inbound buffer never fills.
            let data = self.channel.recv_bytes().await?;
            let mut conn = self.conn.lock().await;
            let mut rbuf = self.rbuf.lock().await;
            let mut cur = Cursor::new(data.as_slice());
            let mut tmp = [0u8; 16384];
            while (cur.position() as usize) < data.len() {
                if conn.read_tls(&mut cur).map_err(|e| {
                    TransportError::ConnectionClosed(format!("cloud_bridge read: {e}"))
                })? == 0
                {
                    break;
                }
                conn.process_new_packets().map_err(|e| {
                    TransportError::ConnectionClosed(format!("cloud_bridge tls: {e}"))
                })?;
                loop {
                    match conn.reader().read(&mut tmp) {
                        Ok(0) => break,
                        Ok(k) => {
                            rbuf.extend_from_slice(&tmp[..k]);
                            if rbuf.len() > MAX_FRAME + HDR {
                                return Err(TransportError::ConnectionClosed(
                                    "cloud_bridge: frame exceeds cap".into(),
                                ));
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            return Err(TransportError::ConnectionClosed(format!(
                                "cloud_bridge plain read: {e}"
                            )))
                        }
                    }
                }
            }
        }
    }

    async fn close(&self) {
        self.channel.close().await;
    }
}
