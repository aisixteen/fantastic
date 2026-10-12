//! The `Sec-WebSocket-Extensions` negotiation, as python's `websockets` runs it:
//! the header grammar of `websockets.headers`, the server side of uvicorn's
//! `ServerPerMessageDeflateFactory()` (every parameter left to the client) and
//! the client side of `enable_client_permessage_deflate` (offer
//! `client_max_window_bits`, accept what the server answers).

use fantastic_bundle::py_repr;

/// The extension's registered name (case-sensitive, as python compares it).
pub const NAME: &str = "permessage-deflate";

/// What python's `websockets` client offers: `ClientPerMessageDeflateFactory`
/// with `client_max_window_bits=True` and nothing else requested.
pub const CLIENT_OFFER: &str = "permessage-deflate; client_max_window_bits";

/// The header's name, as python's errors spell it.
const HEADER: &str = "Sec-WebSocket-Extensions";

/// One side of a negotiated connection — python `PerMessageDeflate`'s four
/// settings. `remote_*` governs what this side DECODES, `local_*` what it
/// ENCODES.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// The peer resets its compressor at every message.
    pub remote_no_context_takeover: bool,
    /// This side resets its compressor at every message.
    pub local_no_context_takeover: bool,
    /// The peer's LZ77 window, in bits (8..=15).
    pub remote_max_window_bits: u8,
    /// This side's LZ77 window, in bits (8..=15).
    pub local_max_window_bits: u8,
}

/// One extension parameter: its name and its value, `None` when it has none.
pub type ExtParam = (String, Option<String>);
/// One offered (or answered) extension: its name and its parameters.
pub type ExtHeader = (String, Vec<ExtParam>);

// ── the header grammar (`websockets.headers`) ───────────────────────

fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-!#$%&'*+.^_`|~".contains(c)
}

/// `InvalidHeaderFormat(name, error, header, pos)`'s text.
fn bad_format(error: &str, header: &str, pos: usize) -> String {
    format!("invalid {HEADER} header: {error} at {pos} in {header}")
}

struct Cursor<'a> {
    chars: Vec<char>,
    header: &'a str,
}

impl Cursor<'_> {
    fn peek(&self, pos: usize) -> Option<char> {
        self.chars.get(pos).copied()
    }

    /// `parse_OWS`.
    fn ows(&self, mut pos: usize) -> usize {
        while matches!(self.peek(pos), Some(' ' | '\t')) {
            pos += 1;
        }
        pos
    }

    /// `parse_token`.
    fn token(&self, pos: usize) -> Result<(String, usize), String> {
        let mut end = pos;
        while self.peek(end).is_some_and(is_tchar) {
            end += 1;
        }
        if end == pos {
            return Err(bad_format("expected token", self.header, pos));
        }
        Ok((self.chars[pos..end].iter().collect(), end))
    }

    /// `parse_quoted_string`: `"…"` of qdtext / quoted-pair, unescaped.
    fn quoted(&self, pos: usize) -> Result<(String, usize), String> {
        let qdtext = |c: char| matches!(c as u32, 0x09 | 0x20..=0x21 | 0x23..=0x5b | 0x5d..=0x7e);
        let pair = |c: char| matches!(c as u32, 0x09 | 0x20..=0x7e | 0x80..=0xff);
        let fail = || Err(bad_format("expected quoted string", self.header, pos));
        if self.peek(pos) != Some('"') {
            return fail();
        }
        let mut out = String::new();
        let mut i = pos + 1;
        loop {
            match self.peek(i) {
                Some('"') => return Ok((out, i + 1)),
                Some('\\') => match self.peek(i + 1) {
                    Some(c) if pair(c) => {
                        out.push(c);
                        i += 2;
                    }
                    _ => return fail(),
                },
                Some(c) if qdtext(c) => {
                    out.push(c);
                    i += 1;
                }
                _ => return fail(),
            }
        }
    }

    /// `parse_extension_item_param`.
    fn param(&self, pos: usize) -> Result<(ExtParam, usize), String> {
        let (name, pos) = self.token(pos)?;
        let mut pos = self.ows(pos);
        let mut value = None;
        if self.peek(pos) == Some('=') {
            pos = self.ows(pos + 1);
            if self.peek(pos) == Some('"') {
                let before = pos;
                let (v, p) = self.quoted(pos)?;
                // RFC 6455 §9.1: the unescaped value MUST be a token.
                if v.is_empty() || !v.chars().all(is_tchar) {
                    return Err(bad_format(
                        "invalid quoted header content",
                        self.header,
                        before,
                    ));
                }
                value = Some(v);
                pos = p;
            } else {
                let (v, p) = self.token(pos)?;
                value = Some(v);
                pos = p;
            }
            pos = self.ows(pos);
        }
        Ok(((name, value), pos))
    }

    /// `parse_extension_item`.
    fn item(&self, pos: usize) -> Result<(ExtHeader, usize), String> {
        let (name, pos) = self.token(pos)?;
        let mut pos = self.ows(pos);
        let mut params = Vec::new();
        while self.peek(pos) == Some(';') {
            pos = self.ows(pos + 1);
            let (p, next) = self.param(pos)?;
            params.push(p);
            pos = next;
        }
        Ok(((name, params), pos))
    }
}

/// python `websockets.headers.parse_extension` — one header value (surrounding
/// whitespace already stripped, as python's header reader does) into the
/// offered extensions, `1#item` with empty list elements ignored.
///
/// # Errors
/// python's `InvalidHeaderFormat` text.
pub fn parse_extension(header: &str) -> Result<Vec<ExtHeader>, String> {
    let c = Cursor {
        chars: header.chars().collect(),
        header,
    };
    let len = c.chars.len();
    let mut pos = 0;
    while c.peek(pos) == Some(',') {
        pos = c.ows(pos + 1);
    }
    let mut items = Vec::new();
    loop {
        let (item, next) = c.item(pos)?;
        items.push(item);
        pos = c.ows(next);
        if pos == len {
            break;
        }
        if c.peek(pos) == Some(',') {
            pos = c.ows(pos + 1);
        } else {
            return Err(bad_format("expected comma", header, pos));
        }
        while c.peek(pos) == Some(',') {
            pos = c.ows(pos + 1);
        }
        if pos == len {
            break;
        }
    }
    Ok(items)
}

/// Every value of the header (python `headers.get_all`), each stripped of
/// `" \t"` as python's reader stores it, parsed and concatenated.
fn parse_all(values: &[&str]) -> Result<Vec<ExtHeader>, String> {
    let mut out = Vec::new();
    for v in values {
        out.extend(parse_extension(v.trim_matches([' ', '\t']))?);
    }
    Ok(out)
}

/// python `build_extension`.
#[must_use]
pub fn build_extension(extensions: &[ExtHeader]) -> String {
    extensions
        .iter()
        .map(|(name, params)| {
            std::iter::once(name.clone())
                .chain(params.iter().map(|(n, v)| match v {
                    None => n.clone(),
                    Some(v) => format!("{n}={v}"),
                }))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ── the parameters (`permessage_deflate._extract_parameters`) ───────

/// `client_max_window_bits` may be offered bare (`True`) in a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bits {
    Bare,
    Of(u8),
}

struct Extracted {
    server_no_context_takeover: bool,
    client_no_context_takeover: bool,
    server_max_window_bits: Option<u8>,
    client_max_window_bits: Option<Bits>,
}

/// A python `NegotiationError` (its subclasses included): the offer is skipped.
struct Negotiation;

fn window_bits(value: Option<&str>) -> Option<u8> {
    // `value in _MAX_WINDOW_BITS_VALUES`: exactly "8" … "15"
    match value {
        Some(v) if (8..=15).any(|b: u8| b.to_string() == v) => v.parse().ok(),
        _ => None,
    }
}

fn extract(params: &[ExtParam], is_server: bool) -> Result<Extracted, Negotiation> {
    let mut x = Extracted {
        server_no_context_takeover: false,
        client_no_context_takeover: false,
        server_max_window_bits: None,
        client_max_window_bits: None,
    };
    for (name, value) in params {
        match name.as_str() {
            "server_no_context_takeover" => {
                if x.server_no_context_takeover || value.is_some() {
                    return Err(Negotiation);
                }
                x.server_no_context_takeover = true;
            }
            "client_no_context_takeover" => {
                if x.client_no_context_takeover || value.is_some() {
                    return Err(Negotiation);
                }
                x.client_no_context_takeover = true;
            }
            "server_max_window_bits" => {
                if x.server_max_window_bits.is_some() {
                    return Err(Negotiation);
                }
                x.server_max_window_bits = Some(window_bits(value.as_deref()).ok_or(Negotiation)?);
            }
            "client_max_window_bits" => {
                if x.client_max_window_bits.is_some() {
                    return Err(Negotiation);
                }
                x.client_max_window_bits = Some(if is_server && value.is_none() {
                    Bits::Bare
                } else {
                    Bits::Of(window_bits(value.as_deref()).ok_or(Negotiation)?)
                });
            }
            _ => return Err(Negotiation),
        }
    }
    Ok(x)
}

fn build_params(
    server_no_context_takeover: bool,
    client_no_context_takeover: bool,
    server_max_window_bits: Option<u8>,
    client_max_window_bits: Option<Bits>,
) -> Vec<ExtParam> {
    let mut out = Vec::new();
    if server_no_context_takeover {
        out.push(("server_no_context_takeover".to_string(), None));
    }
    if client_no_context_takeover {
        out.push(("client_no_context_takeover".to_string(), None));
    }
    if let Some(b) = server_max_window_bits {
        out.push(("server_max_window_bits".to_string(), Some(b.to_string())));
    }
    match client_max_window_bits {
        Some(Bits::Bare) => out.push(("client_max_window_bits".to_string(), None)),
        Some(Bits::Of(b)) => out.push(("client_max_window_bits".to_string(), Some(b.to_string()))),
        None => {}
    }
    out
}

/// python's `zlib.compressobj(wbits=-8)` raises `Invalid initialization
/// option` (raw deflate has no 256-byte window); `PerMessageDeflate.__init__`
/// builds its compressor at once unless it resets per message.
fn compressor_refused(p: &Params) -> bool {
    p.local_max_window_bits == 8 && !p.local_no_context_takeover
}

// ── the server (uvicorn: `ServerPerMessageDeflateFactory()`) ────────

/// Why a server handshake cannot answer `101`: python's own status and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// `400` — the header does not parse (`InvalidHandshake`).
    BadRequest(String),
    /// `500` — the negotiated compressor cannot be built (a `ValueError`).
    ServerError(String),
}

/// The ONE server answer to a client's `Sec-WebSocket-Extensions` values:
/// `Ok(None)` = no extension (nothing offered, or nothing acceptable),
/// `Ok(Some((response_header, params)))` = permessage-deflate, `Err` = the
/// handshake fails with python's status and body.
///
/// uvicorn's factory has every parameter at its default, so the server echoes
/// the client's own requests: `server_no_context_takeover`,
/// `client_no_context_takeover`, `server_max_window_bits=N` and
/// `client_max_window_bits=N` come back as asked; a BARE
/// `client_max_window_bits` is dropped. An offer whose parameters do not parse
/// is skipped; only the first acceptable offer is taken.
///
/// # Errors
/// [`Refused`], with python's response body.
pub fn server_negotiate(values: &[&str]) -> Result<Option<(String, Params)>, Refused> {
    if values.is_empty() {
        return Ok(None);
    }
    let offers = parse_all(values).map_err(|e| {
        Refused::BadRequest(format!("Failed to open a WebSocket connection: {e}.\n"))
    })?;
    let mut answered: Vec<ExtHeader> = Vec::new();
    let mut accepted: Option<Params> = None;
    for (name, params) in offers {
        if name != NAME || accepted.is_some() {
            // another extension, or `skipped duplicate permessage-deflate`
            continue;
        }
        let Ok(x) = extract(&params, true) else {
            continue;
        };
        let client_bits = match x.client_max_window_bits {
            Some(Bits::Bare) | None => None,
            Some(Bits::Of(b)) => Some(Bits::Of(b)),
        };
        let p = Params {
            remote_no_context_takeover: x.client_no_context_takeover,
            local_no_context_takeover: x.server_no_context_takeover,
            remote_max_window_bits: match client_bits {
                Some(Bits::Of(b)) => b,
                _ => 15,
            },
            local_max_window_bits: x.server_max_window_bits.unwrap_or(15),
        };
        if compressor_refused(&p) {
            return Err(Refused::ServerError(
                "Failed to open a WebSocket connection.\nSee server log for more information.\n"
                    .to_string(),
            ));
        }
        answered.push((
            name,
            build_params(
                x.server_no_context_takeover,
                x.client_no_context_takeover,
                x.server_max_window_bits,
                client_bits,
            ),
        ));
        accepted = Some(p);
    }
    Ok(accepted.map(|p| (build_extension(&answered), p)))
}

// ── the client (`enable_client_permessage_deflate`) ─────────────────

/// python's repr of an extension's parameter list: `[('a', None), ('b', '1')]`.
fn params_repr(params: &[ExtParam]) -> String {
    let items: Vec<String> = params
        .iter()
        .map(|(n, v)| {
            format!(
                "({}, {})",
                py_repr(n),
                v.as_deref().map_or("None".to_string(), py_repr)
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// The client's reading of the server's `Sec-WebSocket-Extensions` values,
/// after offering [`CLIENT_OFFER`]: `Ok(None)` = no extension, `Ok(Some)` =
/// permessage-deflate with what the server chose. python's
/// `ClientProtocol.process_extensions` + `process_response_params`.
///
/// # Errors
/// The text of the python exception that fails the connection (an extension it
/// cannot accept is `NegotiationError`; an unparsable header is
/// `InvalidHeaderFormat`; a 256-byte compressor window is zlib's refusal).
pub fn client_accept(values: &[&str]) -> Result<Option<Params>, String> {
    if values.is_empty() {
        return Ok(None);
    }
    let answers = parse_all(values)?;
    let mut accepted: Option<Params> = None;
    for (name, params) in answers {
        let unsupported = || {
            format!(
                "Unsupported extension: name = {name}, params = {}",
                params_repr(&params)
            )
        };
        if name != NAME || accepted.is_some() {
            return Err(unsupported());
        }
        // the request was `client_max_window_bits` (bare) and nothing else, so
        // every response value is acceptable as it stands
        let Ok(x) = extract(&params, false) else {
            return Err(unsupported());
        };
        let p = Params {
            remote_no_context_takeover: x.server_no_context_takeover,
            local_no_context_takeover: x.client_no_context_takeover,
            remote_max_window_bits: x.server_max_window_bits.unwrap_or(15),
            local_max_window_bits: match x.client_max_window_bits {
                Some(Bits::Of(b)) => b,
                _ => 15,
            },
        };
        if compressor_refused(&p) {
            return Err("Invalid initialization option".to_string());
        }
        accepted = Some(p);
    }
    Ok(accepted)
}
