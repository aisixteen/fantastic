//! The frame transform — python `websockets.extensions.permessage_deflate.
//! PerMessageDeflate.decode` / `.encode`, rule for rule: a compressed message's
//! FIRST frame carries RSV1, its continuation frames follow it, control frames
//! pass untouched, every outgoing data frame is compressed (`Z_SYNC_FLUSH`, the
//! trailing `00 00 ff ff` cut on the final frame), and a side that resets per
//! message builds a fresh (de)compressor at each one.

use std::io;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use super::negotiate::Params;

/// What a sync flush ends with, cut from a final frame and put back to decode it.
const EMPTY_UNCOMPRESSED_BLOCK: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// The largest message a peer may inflate to: python's `max_size` on both legs
/// (uvicorn's `ws_max_size`, the ws_bridge client's `max_size=2**24`).
pub const MAX_SIZE: usize = 1 << 24;

/// The continuation opcode.
pub const OP_CONT: u8 = 0x0;

fn is_control(opcode: u8) -> bool {
    opcode & 0x8 != 0
}

fn zlib_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("decompression failed: {e}"),
    )
}

/// zlib's raw inflate takes an 8-bit window (python's `decompressobj(wbits=-8)`);
/// flate2 asks for 9..=15. A 512-byte window inflates every stream a 256-byte
/// one does, so 8 is read with 9.
fn decoder(bits: u8) -> Decompress {
    Decompress::new_with_window_bits(false, bits.max(9))
}

fn encoder(bits: u8) -> io::Result<Compress> {
    if bits < 9 {
        // python `zlib.compressobj(wbits=-8)`: raw deflate has no 256-byte window
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid initialization option",
        ));
    }
    Ok(Compress::new_with_window_bits(
        Compression::default(),
        false,
        bits,
    ))
}

/// One connection's negotiated extension: the peer's stream decoded, this
/// side's encoded.
pub struct PerMessageDeflate {
    params: Params,
    decoder: Option<Decompress>,
    encoder: Option<Compress>,
    /// python `decode_cont_data`: the message being received is compressed.
    decode_cont: bool,
}

impl PerMessageDeflate {
    /// python `PerMessageDeflate.__init__`: the long-lived (de)compressors are
    /// built now; a side that resets per message builds them per message.
    ///
    /// # Errors
    /// zlib's refusal of the window (an 8-bit compressor).
    pub fn new(params: Params) -> io::Result<Self> {
        let decoder =
            (!params.remote_no_context_takeover).then(|| decoder(params.remote_max_window_bits));
        let encoder = if params.local_no_context_takeover {
            None
        } else {
            Some(encoder(params.local_max_window_bits)?)
        };
        Ok(Self {
            params,
            decoder,
            encoder,
            decode_cont: false,
        })
    }

    /// python `decode`: `Some(payload)` = the frame was compressed, here is
    /// its plain payload (RSV1 is to be cleared); `None` = pass it as it is.
    ///
    /// # Errors
    /// A stream that does not inflate, or one that inflates past [`MAX_SIZE`].
    pub fn decode(
        &mut self,
        opcode: u8,
        fin: bool,
        rsv1: bool,
        data: &[u8],
    ) -> io::Result<Option<Vec<u8>>> {
        if is_control(opcode) {
            return Ok(None);
        }
        if opcode == OP_CONT {
            if !self.decode_cont {
                return Ok(None);
            }
            if fin {
                self.decode_cont = false;
            }
        } else {
            if !rsv1 {
                return Ok(None);
            }
            if !fin {
                self.decode_cont = true;
            }
            if self.params.remote_no_context_takeover {
                self.decoder = Some(decoder(self.params.remote_max_window_bits));
            }
        }
        let d = self
            .decoder
            .get_or_insert_with(|| decoder(self.params.remote_max_window_bits));
        let mut input = data.to_vec();
        if fin {
            input.extend_from_slice(&EMPTY_UNCOMPRESSED_BLOCK);
        }
        let out = inflate(d, &input)?;
        if fin && self.params.remote_no_context_takeover {
            self.decoder = None;
        }
        Ok(Some(out))
    }

    /// python `encode`: a data frame's compressed payload, and whether the
    /// frame carries RSV1 (the first frame of a message does); `None` for a
    /// control frame, which passes as it is.
    ///
    /// # Errors
    /// zlib's refusal of the window, or a compressor failure.
    pub fn encode(
        &mut self,
        opcode: u8,
        fin: bool,
        data: &[u8],
    ) -> io::Result<Option<(Vec<u8>, bool)>> {
        if is_control(opcode) {
            return Ok(None);
        }
        if opcode != OP_CONT && self.params.local_no_context_takeover {
            self.encoder = Some(encoder(self.params.local_max_window_bits)?);
        }
        let c = match self.encoder.as_mut() {
            Some(c) => c,
            None => self
                .encoder
                .insert(encoder(self.params.local_max_window_bits)?),
        };
        let mut out = deflate_sync(c, data)?;
        if fin {
            if !out.ends_with(&EMPTY_UNCOMPRESSED_BLOCK) {
                return Err(io::Error::other("a sync flush did not end in 00 00 ff ff"));
            }
            out.truncate(out.len() - EMPTY_UNCOMPRESSED_BLOCK.len());
        }
        if fin && self.params.local_no_context_takeover {
            self.encoder = None;
        }
        Ok(Some((out, opcode != OP_CONT)))
    }
}

/// Inflate ALL of `input` (`decompress(data, max_length)` with python's
/// `max_size` guard).
fn inflate(d: &mut Decompress, input: &[u8]) -> io::Result<Vec<u8>> {
    let start = d.total_in();
    let mut out: Vec<u8> = Vec::with_capacity(input.len().saturating_mul(3).max(256));
    loop {
        let consumed = usize::try_from(d.total_in() - start).unwrap_or(usize::MAX);
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(4096));
        }
        let before = (consumed, out.len());
        let status = d
            .decompress_vec(&input[consumed..], &mut out, FlushDecompress::Sync)
            .map_err(zlib_err)?;
        if out.len() > MAX_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("message too big: over {MAX_SIZE} bytes"),
            ));
        }
        let consumed = usize::try_from(d.total_in() - start).unwrap_or(usize::MAX);
        if status == Status::StreamEnd {
            return Ok(out);
        }
        if consumed == input.len() && out.len() < out.capacity() {
            return Ok(out);
        }
        if (consumed, out.len()) == before && out.len() < out.capacity() {
            return Err(zlib_err("no progress"));
        }
    }
}

/// `compress(data) + flush(Z_SYNC_FLUSH)` — python's two zlib calls, in that
/// order: the input goes in under `Z_NO_FLUSH` first, so even an EMPTY message
/// gets its own sync marker (zlib refuses a second sync flush with nothing new
/// in between).
fn deflate_sync(c: &mut Compress, data: &[u8]) -> io::Result<Vec<u8>> {
    let start = c.total_in();
    let mut out: Vec<u8> = Vec::with_capacity(data.len() / 2 + 64);
    loop {
        if out.capacity() - out.len() < 64 {
            out.reserve(out.capacity().max(1024));
        }
        let consumed = usize::try_from(c.total_in() - start).unwrap_or(usize::MAX);
        c.compress_vec(&data[consumed..], &mut out, FlushCompress::None)
            .map_err(io::Error::other)?;
        let consumed = usize::try_from(c.total_in() - start).unwrap_or(usize::MAX);
        if consumed == data.len() && out.len() < out.capacity() {
            break;
        }
    }
    loop {
        if out.capacity() - out.len() < 64 {
            out.reserve(out.capacity().max(1024));
        }
        c.compress_vec(&[], &mut out, FlushCompress::Sync)
            .map_err(io::Error::other)?;
        if out.len() < out.capacity() {
            return Ok(out);
        }
    }
}
