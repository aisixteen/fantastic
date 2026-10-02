//! Every expected value below is python's: the negotiation vectors were measured
//! against uvicorn 0.46 / websockets 16.0 (the repo venv, 2026-10-02) with raw
//! upgrade requests, the frame payloads were produced by websockets'
//! `PerMessageDeflate.encode`.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn p(rnct: bool, lnct: bool, rbits: u8, lbits: u8) -> Params {
    Params {
        remote_no_context_takeover: rnct,
        local_no_context_takeover: lnct,
        remote_max_window_bits: rbits,
        local_max_window_bits: lbits,
    }
}

/// The `101`'s `Sec-WebSocket-Extensions` value and what it settles, or none.
type Answer<'a> = Option<(&'a str, Params)>;

/// uvicorn's answer (`Sec-WebSocket-Extensions` on the 101) to each offer.
#[test]
fn the_server_answers_each_offer_as_uvicorn_does() {
    let cases: Vec<(&[&str], Answer)> = vec![
        (&[], None),
        (
            &["permessage-deflate"],
            Some(("permessage-deflate", p(false, false, 15, 15))),
        ),
        (
            &["permessage-deflate; client_max_window_bits"],
            Some(("permessage-deflate", p(false, false, 15, 15))),
        ),
        (
            &["permessage-deflate; server_max_window_bits=10"],
            Some((
                "permessage-deflate; server_max_window_bits=10",
                p(false, false, 15, 10),
            )),
        ),
        (
            &["permessage-deflate; client_max_window_bits=9"],
            Some((
                "permessage-deflate; client_max_window_bits=9",
                p(false, false, 9, 15),
            )),
        ),
        (
            &["permessage-deflate; server_no_context_takeover; client_no_context_takeover"],
            Some((
                "permessage-deflate; server_no_context_takeover; client_no_context_takeover",
                p(true, true, 15, 15),
            )),
        ),
        (&["x-webkit-deflate-frame"], None),
        (
            &["foo, permessage-deflate; client_max_window_bits"],
            Some(("permessage-deflate", p(false, false, 15, 15))),
        ),
        (&["permessage-deflate; bogus"], None),
        (
            &["permessage-deflate; server_max_window_bits=16, permessage-deflate"],
            Some(("permessage-deflate", p(false, false, 15, 15))),
        ),
        (&["permessage-deflate; server_max_window_bits"], None),
        (
            &["permessage-deflate; client_max_window_bits=\"12\""],
            Some((
                "permessage-deflate; client_max_window_bits=12",
                p(false, false, 12, 15),
            )),
        ),
        (
            &["permessage-deflate, permessage-deflate; client_no_context_takeover"],
            Some(("permessage-deflate", p(false, false, 15, 15))),
        ),
        (
            &[", permessage-deflate ,"],
            Some(("permessage-deflate", p(false, false, 15, 15))),
        ),
        (&["permessage-deflate; server_no_context_takeover=1"], None),
        (&["PERMESSAGE-DEFLATE"], None),
        // two header lines are one list
        (
            &["foo", "permessage-deflate"],
            Some(("permessage-deflate", p(false, false, 15, 15))),
        ),
    ];
    for (offer, want) in cases {
        let got = server_negotiate(offer).unwrap_or_else(|e| panic!("{offer:?}: {e:?}"));
        let want = want.map(|(h, p)| (h.to_string(), p));
        assert_eq!(got, want, "{offer:?}");
    }
}

/// What uvicorn refuses, and its body: an unparsable header is a `400`, a
/// 256-byte server window a `500` (python's zlib cannot build that compressor).
#[test]
fn the_server_refuses_what_uvicorn_refuses() {
    for (offer, want) in [
        (
            ";;",
            Refused::BadRequest(
                "Failed to open a WebSocket connection: invalid Sec-WebSocket-Extensions header: \
                 expected token at 0 in ;;.\n"
                    .to_string(),
            ),
        ),
        (
            "permessage-deflate;",
            Refused::BadRequest(
                "Failed to open a WebSocket connection: invalid Sec-WebSocket-Extensions header: \
                 expected token at 19 in permessage-deflate;.\n"
                    .to_string(),
            ),
        ),
        (
            "permessage-deflate; a=\"b c\"",
            Refused::BadRequest(
                "Failed to open a WebSocket connection: invalid Sec-WebSocket-Extensions header: \
                 invalid quoted header content at 22 in permessage-deflate; a=\"b c\".\n"
                    .to_string(),
            ),
        ),
        (
            "permessage-deflate; server_max_window_bits=8",
            Refused::ServerError(
                "Failed to open a WebSocket connection.\nSee server log for more information.\n"
                    .to_string(),
            ),
        ),
    ] {
        assert_eq!(server_negotiate(&[offer]), Err(want), "{offer:?}");
    }
    // a 256-byte window the server only builds per message passes the handshake
    assert!(matches!(
        server_negotiate(&[
            "permessage-deflate; server_max_window_bits=8; server_no_context_takeover"
        ]),
        Ok(Some(_))
    ));
}

/// The client offers what `websockets.connect` offers and accepts the answers a
/// python server gives.
#[test]
fn the_client_offers_and_accepts_as_websockets_does() {
    assert_eq!(CLIENT_OFFER, "permessage-deflate; client_max_window_bits");
    assert_eq!(client_accept(&[]), Ok(None));
    // uvicorn's answer
    assert_eq!(
        client_accept(&["permessage-deflate"]),
        Ok(Some(p(false, false, 15, 15)))
    );
    // `websockets.serve`'s default answer (the parity stub peer)
    assert_eq!(
        client_accept(&[
            "permessage-deflate; server_max_window_bits=12; client_max_window_bits=12"
        ]),
        Ok(Some(p(false, false, 12, 12)))
    );
    assert_eq!(
        client_accept(&["permessage-deflate; server_no_context_takeover"]),
        Ok(Some(p(true, false, 15, 15)))
    );
    assert_eq!(
        client_accept(&["foo; a=1"]),
        Err("Unsupported extension: name = foo, params = [('a', '1')]".to_string())
    );
    assert_eq!(
        client_accept(&["permessage-deflate, permessage-deflate"]),
        Err("Unsupported extension: name = permessage-deflate, params = []".to_string())
    );
    assert_eq!(
        client_accept(&["permessage-deflate; client_max_window_bits"]),
        Err(
            "Unsupported extension: name = permessage-deflate, params = \
             [('client_max_window_bits', None)]"
                .to_string()
        )
    );
}

#[test]
fn the_header_grammar_is_websockets_headers() {
    assert_eq!(
        parse_extension("a; b=1; c, d").unwrap(),
        vec![
            (
                "a".to_string(),
                vec![
                    ("b".to_string(), Some("1".to_string())),
                    ("c".to_string(), None)
                ]
            ),
            ("d".to_string(), vec![])
        ]
    );
    assert_eq!(
        parse_extension("a b").unwrap_err(),
        "invalid Sec-WebSocket-Extensions header: expected comma at 2 in a b"
    );
    assert_eq!(
        build_extension(&parse_extension("x ; y = 2 ,z").unwrap()),
        "x; y=2, z"
    );
}

/// python's `PerMessageDeflate.encode` output, decoded here: a context kept
/// across messages, one reset per message, a 4096-window, an empty message and
/// a fragmented one.
#[test]
fn python_s_compressed_frames_inflate_here() {
    let call1 =
        br#"{"type": "call", "id": "1", "target": "readings", "payload": {"type": "reflect"}}"#;
    let call2 =
        br#"{"type": "call", "id": "2", "target": "readings", "payload": {"type": "reflect"}}"#;
    let first = "aa562aa92c4855b252504a4eccc951d25150ca4c01f10c41cc92c4a2f4d41210b728353125332fbd18245a905899939f0852550dd75c949a96939a5ca2545b0b00";
    let big = vec![b'x'; 3000];
    for (params, frames) in [
        (
            p(false, false, 15, 15),
            vec![first, "aac661a011b9060200", "00", "ecc13101000000c2a0dac65f0d0f600000c0bd00"],
        ),
        (
            p(false, false, 12, 12),
            vec![first, "aac661a011b9060200", "00", "ecc13101000000c2a0dac65f0d0f600000c0bd00"],
        ),
        (
            p(true, true, 15, 15),
            vec![
                first,
                "aa562aa92c4855b252504a4eccc951d25150ca4c01f18c40cc92c4a2f4d41210b728353125332fbd18245a905899939f0852550dd75c949a96939a5ca2545b0b00",
                "00",
                "ecc13101000000c2a0da8b6f0d0fa00000807703",
            ],
        ),
    ] {
        let mut pmd = PerMessageDeflate::new(params).unwrap();
        let want: [&[u8]; 4] = [call1, call2, b"", &big];
        for (f, w) in frames.iter().zip(want) {
            let got = pmd.decode(0x1, true, true, &hex(f)).unwrap().expect("compressed");
            assert_eq!(got, w, "{params:?}");
        }
        // an uncompressed message (no RSV1) passes as it is
        assert_eq!(pmd.decode(0x1, true, false, b"plain").unwrap(), None);
        // control frames are never touched
        assert_eq!(pmd.decode(0x9, true, false, b"ping").unwrap(), None);
    }
    let mut pmd = PerMessageDeflate::new(p(false, false, 15, 15)).unwrap();
    let a = pmd
        .decode(0x1, false, true, &hex("ca48cdc9c95700000000ffff"))
        .unwrap()
        .unwrap();
    let b = pmd
        .decode(0x0, true, false, &hex("2acf2fca490100"))
        .unwrap()
        .unwrap();
    assert_eq!([a, b].concat(), b"hello world");
    // after the message, a continuation is no longer this message's
    assert_eq!(pmd.decode(0x0, true, false, b"x").unwrap(), None);
}

/// What this side sends inflates back to the message — every data frame
/// compressed and marked RSV1 on its first frame, the sync marker cut.
#[test]
fn every_outgoing_data_frame_is_compressed() {
    for params in [
        p(false, false, 15, 15),
        p(true, true, 12, 9),
        p(false, false, 9, 9),
    ] {
        let mirror = Params {
            remote_no_context_takeover: params.local_no_context_takeover,
            local_no_context_takeover: params.remote_no_context_takeover,
            remote_max_window_bits: params.local_max_window_bits,
            local_max_window_bits: params.remote_max_window_bits,
        };
        let mut tx = PerMessageDeflate::new(params).unwrap();
        let mut rx = PerMessageDeflate::new(mirror).unwrap();
        for msg in [&b"one message"[..], b"", &vec![7u8; 70_000], b"one message"] {
            let (wire, rsv1) = tx.encode(0x2, true, msg).unwrap().expect("a data frame");
            assert!(rsv1);
            assert!(
                !wire.ends_with(&[0, 0, 0xff, 0xff]),
                "the sync marker is cut"
            );
            assert_eq!(rx.decode(0x2, true, true, &wire).unwrap().unwrap(), msg);
        }
        assert_eq!(tx.encode(0x8, true, b"\x03\xe8").unwrap(), None);
    }
    // a 256-byte compressor is zlib's refusal, built at once or per message
    assert!(PerMessageDeflate::new(p(false, false, 15, 8)).is_err());
    let mut later = PerMessageDeflate::new(p(false, true, 15, 8)).unwrap();
    assert_eq!(
        later.encode(0x1, true, b"x").unwrap_err().to_string(),
        "Invalid initialization option"
    );
}

/// A frame head + payload, masked when `mask` is given.
fn frame(b0: u8, mask: Option<[u8; 4]>, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![b0];
    let m = if mask.is_some() { 0x80 } else { 0 };
    assert!(payload.len() < 126);
    out.push(m | payload.len() as u8);
    match mask {
        None => out.extend_from_slice(payload),
        Some(k) => {
            out.extend_from_slice(&k);
            out.extend(payload.iter().enumerate().map(|(i, b)| b ^ k[i & 3]));
        }
    }
    out
}

/// End to end over a socket pair: a client frame leaves deflated (RSV1, the same
/// mask), the server stream hands its reader the original frame; a ping passes
/// untouched.
#[tokio::test]
async fn frames_cross_the_stream_deflated_and_come_out_whole() {
    let (a, b) = tokio::io::duplex(64 * 1024);
    let params = p(false, false, 15, 15);
    let mut client = DeflateStream::negotiated(a, Some(params)).unwrap();
    let mut server = DeflateStream::negotiated(b, Some(params)).unwrap();
    let k = [1, 2, 3, 4];
    let text = frame(0x81, Some(k), b"hello hello hello");
    let ping = frame(0x89, Some(k), b"p");
    client.write_all(&text).await.unwrap();
    client.write_all(&ping).await.unwrap();
    client.flush().await.unwrap();
    let mut got = vec![0u8; text.len() + ping.len()];
    server.read_exact(&mut got).await.unwrap();
    assert_eq!(got, [text, ping].concat());
}

/// The client sniffs the server's 101: what it negotiated applies to the very
/// first frame, even one that arrives in the same read as the head.
#[tokio::test]
async fn the_client_negotiates_from_the_response_head() {
    let (a, mut b) = tokio::io::duplex(64 * 1024);
    let mut client = DeflateStream::client(a);
    client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    client.flush().await.unwrap();
    let mut req = vec![0u8; 18];
    b.read_exact(&mut req).await.unwrap();
    assert_eq!(
        req, b"GET / HTTP/1.1\r\n\r\n",
        "the request goes out as written"
    );
    let head = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                 Sec-WebSocket-Extensions: permessage-deflate; server_max_window_bits=12; \
                 client_max_window_bits=12\r\n\r\n";
    // python's first message, as `websockets.serve` sends it (unmasked)
    let mut pmd = PerMessageDeflate::new(p(false, false, 12, 12)).unwrap();
    let (wire, _) = pmd.encode(0x1, true, b"{\"ok\": 1}").unwrap().unwrap();
    let mut first = vec![0xc1, wire.len() as u8];
    first.extend_from_slice(&wire);
    b.write_all(&[&head[..], &first].concat()).await.unwrap();
    let mut got = vec![0u8; head.len()];
    client.read_exact(&mut got).await.unwrap();
    assert_eq!(got, head);
    assert!(client.deflating());
    let mut got = vec![0u8; 2 + 9];
    client.read_exact(&mut got).await.unwrap();
    assert_eq!(
        got,
        frame(0x81, None, b"{\"ok\": 1}"),
        "RSV1 cleared, payload inflated"
    );
}
