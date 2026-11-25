//! The hold-back scanner of a reader whose calls are HELD (#766, 1.4, 1.5 step 4).
//!
//! A type whose call text is not the internal form cannot forward a call as it streams:
//! the chat maps a whole call to internal call text, so the call is held from its opener
//! to the end its [`Body`] finds, and comes out as one piece. Everything else streams
//! live:
//! - reasoning spans (the provider's [`THINK_OPEN`] / [`THINK_CLOSE`], the same for every
//!   type) are forwarded as text and never read for calls; an unclosed span runs to the
//!   end of the stream as text;
//! - outside reasoning, an opener is read wherever it stands. Only the bytes that could
//!   still become an opener or a reasoning marker are held back, and they are released as
//!   text as soon as they cannot (a look-alike such as `<tool_calls_x`);
//! - inside a held call the reasoning markers are value bytes: the body alone says where
//!   the call ends;
//! - a stream that ends inside a held call gives [`Piece::Open`].
//!
//! The scanner knows the opener only. Where a call ends is the type's own grammar (a
//! closer found by reading the call's JSON and its block, for `hermes`), so it is a
//! [`Body`] the reader hands in. The reader turns each [`Piece`] into its `Out` values.
//!
//! Every byte fed comes out exactly once, in order, in one piece; nothing is normalised.
//! The pieces of a stream do not depend on how it was split into deltas, except that
//! text may come out in more pieces when it arrives in more deltas.

use crate::think::{THINK_CLOSE, THINK_OPEN};

/// The grammar of one held call after its opener: where the call ends.
pub(crate) trait Body: Send {
    /// The next bytes of the call, after its opener. `Some(n)`: the call ends `n` bytes
    /// into `more` (its closer included); the rest of `more` is outside the call again.
    /// `None`: the call goes on past `more`. A body keeps whatever state it needs across
    /// calls (a closer split between two deltas, a string being read).
    fn feed(&mut self, more: &str) -> Option<usize>;
}

/// One piece of a stream as the scanner reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    /// Bytes to forward now: prose, reasoning with its markers, released look-alikes.
    Text(String),
    /// One whole held call, its opener and its end included, byte for byte.
    Call(String),
    /// The stream ended inside a held call: its opener and every byte after it.
    Open(String),
}

/// The scanner of one answer: its opener and a fresh [`Body`] per call.
#[derive(Debug)]
pub(crate) struct Scanner<B: Body + Default> {
    /// The type's call opener, written whole (ASCII).
    opener: &'static str,
    /// Bytes outside a call not forwarded yet: at most a tail that could still begin an
    /// opener or a reasoning marker.
    held: String,
    /// Inside an unclosed reasoning span.
    think: bool,
    /// The held call: its bytes so far (the opener included) and its body.
    call: Option<(String, B)>,
}

impl<B: Body + Default> Scanner<B> {
    /// A scanner for one answer whose calls open with `opener`.
    pub(crate) fn new(opener: &'static str) -> Self {
        debug_assert!(opener.is_ascii() && !opener.is_empty());
        Self {
            opener,
            held: String::new(),
            think: false,
            call: None,
        }
    }

    /// One delta of the model's raw text in; the pieces that can be told now out.
    pub(crate) fn feed(&mut self, delta: &str) -> Vec<Piece> {
        let mut out = Vec::new();
        let mut rest = delta.to_string();
        loop {
            if let Some((text, body)) = self.call.as_mut() {
                match body.feed(&rest) {
                    Some(n) => {
                        let n = n.min(rest.len());
                        debug_assert!(rest.is_char_boundary(n), "a body ends on a char boundary");
                        text.push_str(&rest[..n]);
                        let (text, _) = self.call.take().expect("a held call");
                        out.push(Piece::Call(text));
                        rest.drain(..n);
                    }
                    None => {
                        text.push_str(&rest);
                        return out;
                    }
                }
            }
            self.held.push_str(&rest);
            if !self.scan(&mut out) {
                return out;
            }
            // an opener at the front of `held`: the call starts, the bytes after the
            // opener go to its body
            rest = self.held.split_off(self.opener.len());
            self.held.clear();
            self.call = Some((self.opener.to_string(), B::default()));
        }
    }

    /// The stream ended: whatever was held.
    pub(crate) fn finish(&mut self) -> Vec<Piece> {
        let mut out = Vec::new();
        if let Some((text, _)) = self.call.take() {
            out.push(Piece::Open(text));
        }
        let rest = std::mem::take(&mut self.held);
        push_text(&mut out, &rest);
        out
    }

    /// Read `self.held` outside a call: forward every byte that is settled, follow the
    /// reasoning markers, keep a tail that may still become a marker or the opener.
    /// True: an opener was found; everything before it was forwarded and removed, so the
    /// opener is at the front of `self.held`.
    fn scan(&mut self, out: &mut Vec<Piece>) -> bool {
        loop {
            if self.think {
                match self.held.find(THINK_CLOSE) {
                    Some(i) => {
                        let end = i + THINK_CLOSE.len();
                        let text: String = self.held.drain(..end).collect();
                        push_text(out, &text);
                        self.think = false;
                    }
                    None => {
                        let keep = held_tail(&self.held, &[THINK_CLOSE]);
                        let text: String = self.held.drain(..self.held.len() - keep).collect();
                        push_text(out, &text);
                        return false;
                    }
                }
                continue;
            }
            let think = self.held.find(THINK_OPEN);
            let call = self.held.find(self.opener);
            match (think, call) {
                (Some(t), c) if c.is_none_or(|c| t < c) => {
                    let end = t + THINK_OPEN.len();
                    let text: String = self.held.drain(..end).collect();
                    push_text(out, &text);
                    self.think = true;
                }
                (_, Some(c)) => {
                    let text: String = self.held.drain(..c).collect();
                    push_text(out, &text);
                    return true;
                }
                (_, None) => {
                    let keep = held_tail(&self.held, &[THINK_OPEN, self.opener]);
                    let text: String = self.held.drain(..self.held.len() - keep).collect();
                    push_text(out, &text);
                    return false;
                }
            }
        }
    }
}

/// Append `text` to `out`, joined to a text piece right before it.
fn push_text(out: &mut Vec<Piece>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Piece::Text(last)) = out.last_mut() {
        last.push_str(text);
    } else {
        out.push(Piece::Text(text.to_string()));
    }
}

/// How many trailing bytes of `buf` could be the start of one of `tags`: held back until
/// the next delta says whether they are markup or text. The tags are ASCII, so a matching
/// tail is ASCII and starts on a char boundary.
fn held_tail(buf: &str, tags: &[&str]) -> usize {
    let b = buf.as_bytes();
    let mut best = 0;
    for tag in tags {
        let t = tag.as_bytes();
        for n in (1..=(t.len() - 1).min(b.len())).rev() {
            if t.starts_with(&b[b.len() - n..]) {
                best = best.max(n);
                break;
            }
        }
    }
    best
}
