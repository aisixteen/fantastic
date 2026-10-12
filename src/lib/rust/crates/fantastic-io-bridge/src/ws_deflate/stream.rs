//! [`DeflateStream`] — the byte stream under a WebSocket, with permessage-deflate
//! applied to the frames crossing it. `tungstenite` (which has no extension
//! support and refuses RSV1) runs on top unchanged: what it reads has been
//! inflated and its RSV1 cleared, what it writes is deflated and marked on the
//! way out.

use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::codec::{PerMessageDeflate, MAX_SIZE};
use super::negotiate::{client_accept, Params};

/// One frame's head as RFC 6455 §5.2 lays it out, and where its payload is.
struct Head {
    fin: bool,
    rsv1: bool,
    opcode: u8,
    mask: Option<[u8; 4]>,
    /// header length (payload starts here)
    hlen: usize,
    /// payload length
    plen: usize,
}

/// A complete frame at the front of `buf`, or `None` for "not all here yet".
fn parse_head(buf: &[u8]) -> io::Result<Option<Head>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buf[0], buf[1]);
    let masked = b1 & 0x80 != 0;
    let mut hlen = 2;
    let plen = match b1 & 0x7f {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            hlen = 4;
            usize::from(u16::from_be_bytes([buf[2], buf[3]]))
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            hlen = 10;
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[2..10]);
            usize::try_from(u64::from_be_bytes(b)).unwrap_or(usize::MAX)
        }
        n => usize::from(n),
    };
    if plen > MAX_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too big: {plen} bytes"),
        ));
    }
    let mask = if masked {
        if buf.len() < hlen + 4 {
            return Ok(None);
        }
        let mut m = [0u8; 4];
        m.copy_from_slice(&buf[hlen..hlen + 4]);
        hlen += 4;
        Some(m)
    } else {
        None
    };
    if buf.len() < hlen + plen {
        return Ok(None);
    }
    Ok(Some(Head {
        fin: b0 & 0x80 != 0,
        rsv1: b0 & 0x40 != 0,
        opcode: b0 & 0x0f,
        mask,
        hlen,
        plen,
    }))
}

fn apply_mask(data: &mut [u8], mask: [u8; 4]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
}

/// Write one frame: `b0`'s FIN/RSV/opcode as given (RSV1 per `rsv1`), the
/// payload masked with `mask` when the frame was masked.
fn write_frame(out: &mut Vec<u8>, b0: u8, rsv1: bool, mask: Option<[u8; 4]>, payload: &[u8]) {
    out.push(if rsv1 { b0 | 0x40 } else { b0 & !0x40 });
    let m = if mask.is_some() { 0x80 } else { 0 };
    let len = payload.len();
    if len < 126 {
        out.push(m | len as u8);
    } else if len <= usize::from(u16::MAX) {
        out.push(m | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(m | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    match mask {
        None => out.extend_from_slice(payload),
        Some(k) => {
            out.extend_from_slice(&k);
            let start = out.len();
            out.extend_from_slice(payload);
            apply_mask(&mut out[start..], k);
        }
    }
}

/// Which way a frame crosses the stream.
#[derive(Clone, Copy)]
enum Way {
    In,
    Out,
}

/// Transform every complete frame at the front of `src` into `dst`.
fn pump(
    pmd: &mut PerMessageDeflate,
    way: Way,
    src: &mut Vec<u8>,
    dst: &mut Vec<u8>,
) -> io::Result<()> {
    let mut at = 0;
    while let Some(h) = parse_head(&src[at..])? {
        let frame = &src[at..at + h.hlen + h.plen];
        let mut payload = frame[h.hlen..].to_vec();
        if let Some(k) = h.mask {
            apply_mask(&mut payload, k);
        }
        let changed = match way {
            Way::In => pmd
                .decode(h.opcode, h.fin, h.rsv1, &payload)?
                .map(|p| (p, false)),
            Way::Out => pmd.encode(h.opcode, h.fin, &payload)?,
        };
        match changed {
            Some((p, rsv1)) => write_frame(dst, frame[0], rsv1, h.mask, &p),
            None => dst.extend_from_slice(frame),
        }
        at += h.hlen + h.plen;
    }
    src.drain(..at);
    Ok(())
}

/// The client half before the handshake is done: bytes pass untouched until
/// the server's response head has gone by; THAT head says what was negotiated.
struct Sniff {
    head: Vec<u8>,
}

/// The handshake head is never this long.
const MAX_HEAD: usize = 64 * 1024;

/// The response head's `Sec-WebSocket-Extensions` values, when the status is
/// `101` — a refused upgrade negotiates nothing (tungstenite reports it), and
/// neither does a head that does not parse (tungstenite says what is wrong).
fn response_extensions(head: &[u8]) -> Option<Vec<String>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);
    match resp.parse(head) {
        Ok(httparse::Status::Complete(_)) if resp.code == Some(101) => Some(
            resp.headers
                .iter()
                .filter(|h| h.name.eq_ignore_ascii_case("sec-websocket-extensions"))
                .map(|h| String::from_utf8_lossy(h.value).into_owned())
                .collect(),
        ),
        _ => None,
    }
}

/// The byte stream under one WebSocket connection, permessage-deflate applied
/// to its frames once negotiated.
pub struct DeflateStream<S> {
    inner: S,
    pmd: Option<PerMessageDeflate>,
    sniff: Option<Sniff>,
    rd_in: Vec<u8>,
    rd_out: Vec<u8>,
    rd_pos: usize,
    rd_eof: bool,
    wr_in: Vec<u8>,
    wr_out: Vec<u8>,
    wr_pos: usize,
    scratch: Box<[u8]>,
}

impl<S> DeflateStream<S> {
    fn with(inner: S, pmd: Option<PerMessageDeflate>, sniff: Option<Sniff>) -> Self {
        Self {
            inner,
            pmd,
            sniff,
            rd_in: Vec::new(),
            rd_out: Vec::new(),
            rd_pos: 0,
            rd_eof: false,
            wr_in: Vec::new(),
            wr_out: Vec::new(),
            wr_pos: 0,
            scratch: vec![0u8; 16 * 1024].into_boxed_slice(),
        }
    }

    /// A connection whose handshake is done: the extension as it negotiated it
    /// (`None` = none — the bytes pass untouched). A server's upgraded socket.
    ///
    /// # Errors
    /// zlib's refusal of the negotiated window.
    pub fn negotiated(inner: S, negotiated: Option<Params>) -> io::Result<Self> {
        let pmd = negotiated.map(PerMessageDeflate::new).transpose()?;
        Ok(Self::with(inner, pmd, None))
    }

    /// A client's connection, BEFORE its handshake: the request goes out
    /// untouched, and the server's response head — read through here — decides
    /// whether the frames after it are deflated. The request must offer
    /// [`super::CLIENT_OFFER`].
    pub fn client(inner: S) -> Self {
        Self::with(inner, None, Some(Sniff { head: Vec::new() }))
    }

    /// Whether permessage-deflate is in force on this connection.
    pub fn deflating(&self) -> bool {
        self.pmd.is_some()
    }

    /// The stream under the frames.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> DeflateStream<S> {
    /// Push pending outbound bytes into the inner stream.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.wr_pos < self.wr_out.len() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.wr_out[self.wr_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.wr_pos += n;
        }
        self.wr_out.clear();
        self.wr_pos = 0;
        Poll::Ready(Ok(()))
    }

    /// Bytes read off the wire while the client handshake is in flight: pass
    /// the response head through, then negotiate from it; whatever followed the
    /// head is frames.
    fn take_sniffed(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(sniff) = self.sniff.as_mut() else {
            self.rd_in.extend_from_slice(bytes);
            return Ok(());
        };
        sniff.head.extend_from_slice(bytes);
        let Some(end) = sniff.head.windows(4).position(|w| w == b"\r\n\r\n") else {
            if sniff.head.len() > MAX_HEAD {
                // not a handshake this side recognises — tungstenite says why
                let head = std::mem::take(&mut sniff.head);
                self.sniff = None;
                self.rd_out.extend_from_slice(&head);
            }
            return Ok(());
        };
        let mut head = std::mem::take(&mut sniff.head);
        self.sniff = None;
        let rest = head.split_off(end + 4);
        if let Some(values) = response_extensions(&head) {
            let values: Vec<&str> = values.iter().map(String::as_str).collect();
            let negotiated = client_accept(&values).map_err(io::Error::other)?;
            self.pmd = negotiated.map(PerMessageDeflate::new).transpose()?;
        }
        self.rd_out.extend_from_slice(&head);
        self.rd_in.extend_from_slice(&rest);
        Ok(())
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for DeflateStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // no extension, nothing held back: the bytes are the inner stream's
        if this.sniff.is_none()
            && this.pmd.is_none()
            && this.rd_pos == this.rd_out.len()
            && this.rd_in.is_empty()
        {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        loop {
            if this.rd_pos < this.rd_out.len() {
                let n = buf.remaining().min(this.rd_out.len() - this.rd_pos);
                buf.put_slice(&this.rd_out[this.rd_pos..this.rd_pos + n]);
                this.rd_pos += n;
                if this.rd_pos == this.rd_out.len() {
                    this.rd_out.clear();
                    this.rd_pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            if this.sniff.is_none() {
                match this.pmd.as_mut() {
                    Some(pmd) => pump(pmd, Way::In, &mut this.rd_in, &mut this.rd_out)?,
                    None => this.rd_out.append(&mut this.rd_in),
                }
                if !this.rd_out.is_empty() {
                    continue;
                }
            }
            if this.rd_eof {
                // a frame cut short by the end of the stream reaches tungstenite
                // as it is, and it reports the truncation
                if !this.rd_in.is_empty() {
                    this.rd_out.append(&mut this.rd_in);
                    continue;
                }
                return Poll::Ready(Ok(()));
            }
            let mut rb = ReadBuf::new(&mut this.scratch);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
            if rb.filled().is_empty() {
                this.rd_eof = true;
                if let Some(s) = this.sniff.take() {
                    this.rd_out.extend_from_slice(&s.head);
                }
                continue;
            }
            let got = rb.filled().to_vec();
            this.take_sniffed(&got)?;
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for DeflateStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        // the handshake request, and every byte of a connection with no
        // extension, goes out as written (after anything queued before it)
        if this.sniff.is_some() || this.pmd.is_none() {
            ready!(this.poll_drain(cx))?;
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        // backpressure: a full outbound queue waits for the wire
        if this.wr_out.len() - this.wr_pos > 256 * 1024 {
            ready!(this.poll_drain(cx))?;
        }
        if let Some(pmd) = this.pmd.as_mut() {
            this.wr_in.extend_from_slice(buf);
            pump(pmd, Way::Out, &mut this.wr_in, &mut this.wr_out)?;
        }
        // start the bytes on their way; a full socket finishes them at flush
        if let Poll::Ready(Err(e)) = this.poll_drain(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}
