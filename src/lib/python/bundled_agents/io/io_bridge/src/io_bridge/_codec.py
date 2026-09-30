"""io_bridge frame codec — the ONE binary-safe wire encoding shared by every
transport (web_ws WS frames, ws_bridge WSTransport, relay_connector relay frames).

A bridge frame is a JSON envelope (`{type, target, payload, id}` / `{type:reply,
id, data}` / …). JSON cannot hold a raw `bytes` value — so a frame whose payload
carries bytes (a `read_stream` chunk, an audio buffer) is encoded as a **binary
frame**:

    [ 4-byte BE uint32 H | H-byte JSON header | M-byte raw body ]

The header is the envelope with the single bytes value replaced by `null` and an
extra `_binary_path: "<dotted.path>"` naming where it lived; the receiver parses the
header, reads the trailing M bytes as the body, and sets it back at that path. A
frame with NO bytes is plain UTF-8 JSON. This is how raw bytes ride the wire WITHOUT
base64 — the +33% size + encode/decode tax is gone.

The text/binary distinction is carried by the transport, NOT guessed here. Every
transport today is WS-based (web_ws, ws_bridge's WSTransport, relay_connector over
the relay) and uses the WS frame TYPE — a text frame arrives as `str`, a binary
frame as `bytes`; `decode_frame` dispatches on that. The relay forwards the WS
frame kind end-to-end, so a tunneled binary chunk stays a binary frame.

Only ONE bytes value per frame is supported (the stream protocol carries exactly one
chunk per frame); the first found in a depth-first walk wins.
"""

from __future__ import annotations

import copy
import json
import struct
from typing import Any


def find_bytes_path(obj: Any, prefix: str = "") -> tuple[str, bytes] | None:
    """Walk obj; return (dotted_path, value) of the first bytes value, or None."""
    if isinstance(obj, (bytes, bytearray)):
        return (prefix, bytes(obj))
    if isinstance(obj, dict):
        for k, v in obj.items():
            p = f"{prefix}.{k}" if prefix else k
            r = find_bytes_path(v, p)
            if r is not None:
                return r
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            p = f"{prefix}.{i}" if prefix else str(i)
            r = find_bytes_path(v, p)
            if r is not None:
                return r
    return None


def set_path(obj: Any, path: str, value: Any) -> None:
    parts = path.split(".")
    for p in parts[:-1]:
        obj = obj[int(p)] if isinstance(obj, list) else obj[p]
    last = parts[-1]
    if isinstance(obj, list):
        obj[int(last)] = value
    else:
        obj[last] = value


def encode_frame(envelope: dict) -> tuple[bytes, bool]:
    """Serialize an envelope for the wire. Returns (wire_bytes, is_binary). If any
    bytes value is present, a binary frame `[4-byte BE len | header JSON | body]`;
    otherwise UTF-8 JSON. (Even the text case returns bytes — a WS transport decodes
    it back to a `str` for a text frame; a byte-stream transport sends it as-is.)"""
    found = find_bytes_path(envelope)
    if found is None:
        return json.dumps(envelope, default=str).encode("utf-8"), False
    path, body = found
    head_obj = copy.deepcopy(envelope)
    set_path(head_obj, path, None)
    head_obj["_binary_path"] = path
    head_bytes = json.dumps(head_obj, default=str).encode("utf-8")
    return struct.pack(">I", len(head_bytes)) + head_bytes + body, True


def _no_constant(name: str):
    raise ValueError(f"{name} is not JSON")


def loads(text: str | bytes):
    """JSON off the wire, STRICTLY: `NaN`, `Infinity` and `-Infinity` are not JSON
    (RFC 8259), and python's `json.loads` alone accepts them — a peer could plant a
    value no other runtime can even represent (parity 2026-09-30: python took a
    `NaN` REST body that rust and swift refuse). Refused as a `JSONDecodeError`, so
    every caller's existing "not valid JSON" path answers it."""
    try:
        return json.loads(text, parse_constant=_no_constant)
    except ValueError as e:
        if isinstance(e, json.JSONDecodeError):
            raise
        doc = text if isinstance(text, str) else ""
        raise json.JSONDecodeError(str(e), doc, 0) from None


def decode_frame(data: bytes | str) -> dict:
    """Parse a wire frame back into an envelope. `str` ⇒ a text (JSON) frame; `bytes`
    ⇒ a binary frame with the raw body restored at `_binary_path`.

    A frame arrives from a PEER, so every length in it is hostile until checked: a
    4-byte prefix claiming 4 GiB of header on a 3-byte frame used to slice garbage
    and raise `struct.error`/`JSONDecodeError` out of whichever read loop called us.
    The bounds are checked here, once, and every malformed frame leaves as the SAME
    `ValueError` — so a caller can catch one thing and keep its socket."""
    if isinstance(data, str):
        try:
            return loads(data)
        except Exception as e:
            raise ValueError(f"bad text frame: {e}") from e
    if len(data) < 4:
        raise ValueError(f"binary frame shorter than its 4-byte header ({len(data)}B)")
    head_len = struct.unpack(">I", data[:4])[0]
    if 4 + head_len > len(data):
        raise ValueError(
            f"binary frame header claims {head_len}B but only "
            f"{len(data) - 4}B follow the length prefix"
        )
    try:
        head = loads(data[4 : 4 + head_len].decode("utf-8"))
    except Exception as e:
        raise ValueError(f"bad binary frame header: {e}") from e
    body = data[4 + head_len :]
    path = head.pop("_binary_path", None) if isinstance(head, dict) else None
    if path is not None:
        try:
            set_path(head, path, body)
        except Exception as e:
            raise ValueError(f"bad _binary_path {path!r}: {e}") from e
    return head
