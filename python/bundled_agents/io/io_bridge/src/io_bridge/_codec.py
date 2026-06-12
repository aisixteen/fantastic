"""io_bridge frame codec — the ONE binary-safe wire encoding shared by every
transport (web_ws WS frames, ws_bridge WSTransport, cloud_bridge TLS records).

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

The text/binary distinction is carried by the transport, NOT guessed here:
  - WS transports (web_ws, WSTransport) use the WS frame TYPE — a text frame arrives
    as `str`, a binary frame as `bytes`; `decode_frame` dispatches on that.
  - byte-stream transports (cloud_bridge, inside TLS) prepend a 1-byte discriminator
    to their length-delimited record and hand `decode_frame` the right type.

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


def decode_frame(data: bytes | str) -> dict:
    """Parse a wire frame back into an envelope. `str` ⇒ a text (JSON) frame; `bytes`
    ⇒ a binary frame with the raw body restored at `_binary_path`."""
    if isinstance(data, str):
        return json.loads(data)
    head_len = struct.unpack(">I", data[:4])[0]
    head = json.loads(data[4 : 4 + head_len].decode("utf-8"))
    body = data[4 + head_len :]
    path = head.pop("_binary_path", None)
    if path is not None:
        set_path(head, path, body)
    return head
