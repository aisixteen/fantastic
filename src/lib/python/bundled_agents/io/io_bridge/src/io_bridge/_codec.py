"""io_bridge frame codec — the ONE binary-safe wire encoding shared by every
transport (web_ws WS frames, ws_bridge WSTransport, relay_connector relay frames).

A bridge frame is a JSON envelope (`{type, target, payload, id}` / `{type:reply,
id, data}` / …). JSON cannot hold a raw `bytes` value — so a frame whose payload
carries bytes (a `read_stream` chunk, an audio buffer) is encoded as a **binary
frame**:

    [ 4-byte BE uint32 H | H-byte JSON header | M-byte raw body ]

The header is the envelope with the single bytes value replaced by `null` and an
extra `_binary_path` naming where it lived; the receiver parses the header, reads
the trailing M bytes as the body, and sets it back at that path. A frame with NO
bytes is plain UTF-8 JSON. This is how raw bytes ride the wire WITHOUT base64 — the
+33% size + encode/decode tax is gone.

`_binary_path` is a JSON ARRAY of steps from the envelope root: a STRING step is an
object key, taken literally (a key may hold `.`); an INTEGER step is a list index.
`["payload", "bytes"]`, `["payload", "a.b"]` (the top-level payload key `"a.b"`),
`["data", "items", 0, "data"]`. A dotted string could not tell the key `"a.b"` from
the nested `a` → `b`, so a payload whose bytes sat at a literal dotted key (web_rest's
`X-Fantastic-Bytes-Field` binds one) arrived rebuilt as a nested slot.

The text/binary distinction is carried by the transport, NOT guessed here. Every
transport today is WS-based (web_ws, ws_bridge's WSTransport, relay_connector over
the relay) and uses the WS frame TYPE — a text frame arrives as `str`, a binary
frame as `bytes`; `decode_frame` dispatches on that. The relay forwards the WS
frame kind end-to-end, so a tunneled binary chunk stays a binary frame.

ONE bytes value per frame (the stream protocol carries exactly one chunk per frame;
rust replies hold one body by type). An envelope holding MORE is refused
by `encode_frame` (`FrameError`, naming each path) — never written with the second
value stringified (`json.dumps(default=str)` → `"b'…'"`), a corrupted value the
receiver could not tell from data. A sender passes what it sends through
`sendable` first: a reply or an event that cannot be carried goes out as its routing
keys and a refusal where its body was, so the receiver learns why; a frame the
sender originates (a call) is refused at the sender.
"""

from __future__ import annotations

import copy
import json
import math
import struct
from typing import Any

from kernel._refusal import TRANSPORT_ERROR, refusal

#: THE ONE FRAME CAP every wire leg holds: 16 MiB — a WS frame (`ws_bridge`'s
#: `max_size`, the relay's `MAX_FRAME`, uvicorn's default `ws_max_size` under
#: `web_ws`), an exec_bridge control-fd frame, and a web_rest request body (the
#: http face of the same envelope). Larger data moves in chunks (`write_stream`).
MAX_FRAME = 16 * 1024 * 1024


def find_bytes_path(
    obj: Any, prefix: tuple = ()
) -> tuple[list[str | int], bytes] | None:
    """Walk obj depth-first; return `(path, value)` of the first bytes value — the
    path as `_binary_path` spells it (a str per object key, an int per list
    index) — or None."""
    if isinstance(obj, (bytes, bytearray)):
        return (list(prefix), bytes(obj))
    if isinstance(obj, dict):
        for k, v in obj.items():
            r = find_bytes_path(v, (*prefix, k))
            if r is not None:
                if not isinstance(k, str):
                    # JSON writes the key as a string; a step that is not the
                    # key the receiver will read is refused here, at the sender.
                    raise TypeError(
                        f"the bytes sit under the non-string key {k!r}; "
                        "a frame's object keys are strings"
                    )
                return r
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            r = find_bytes_path(v, (*prefix, i))
            if r is not None:
                return r
    return None


def set_path(obj: Any, path: Any, value: Any) -> None:
    """Put `value` at `path` (a `_binary_path`: a non-empty list of steps) in obj.

    The path comes off the wire, so it is checked step by step: at an object the
    step is a string key (every key before the last must exist; the last is set),
    at a list a non-negative integer index inside it. Anything else raises
    `ValueError` naming the step."""
    if not isinstance(path, list):
        raise ValueError("not a list of steps")
    if not path:
        raise ValueError("an empty path names no slot")
    for n, step in enumerate(path):
        last = n == len(path) - 1
        if isinstance(obj, dict):
            if not isinstance(step, str):
                raise ValueError(f"step {step!r} at an object is not a string key")
            if last:
                obj[step] = value
            elif step in obj:
                obj = obj[step]
            else:
                raise ValueError(f"no key {step!r}")
        elif isinstance(obj, list):
            if type(step) is not int or step < 0:
                raise ValueError(
                    f"step {step!r} at a list is not a non-negative integer index"
                )
            if step >= len(obj):
                raise ValueError(
                    f"index {step} is past the end of a list of {len(obj)}"
                )
            if last:
                obj[step] = value
            else:
                obj = obj[step]
        else:
            raise ValueError(f"step {step!r} goes into a {_kind(obj)}, not a container")


def _kind(v: Any) -> str:
    """The JSON name of a scalar, for an error."""
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "boolean"
    if isinstance(v, (int, float)):
        return "number"
    return "string"


class FrameError(ValueError):
    """An envelope the wire cannot carry — named, never sent corrupted."""


def bytes_paths(obj: Any, prefix: tuple = ()) -> list[list[str | int]]:
    """Every bytes value's path in `obj`, depth-first (as `_binary_path` spells it)."""
    if isinstance(obj, (bytes, bytearray)):
        return [list(prefix)]
    out: list[list[str | int]] = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            out += bytes_paths(v, (*prefix, k))
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            out += bytes_paths(v, (*prefix, i))
    return out


def _one_body(envelope: Any) -> None:
    paths = bytes_paths(envelope)
    if len(paths) > 1:
        raise FrameError(
            f"a frame carries ONE binary body — this one holds {len(paths)}, at "
            + ", ".join(json.dumps(p) for p in paths)
        )


#: The keys that route a frame — kept on what a sender sends instead.
_ROUTING = ("type", "op", "id", "rid", "src", "target")


def sendable(envelope: dict) -> dict:
    """What a sender may put on the wire for `envelope`: itself when it holds at
    most one bytes value; for a REPLY (`data`) or an EVENT (`payload` of an
    `event`) that holds more, its routing keys and a `transport_error` refusal
    where its body was. Anything else that holds more (a call the sender
    originates) raises `FrameError` — refused where it starts."""
    try:
        _one_body(envelope)
        return envelope
    except FrameError as e:
        body = refusal(TRANSPORT_ERROR, str(e))
        out = {k: envelope[k] for k in _ROUTING if k in envelope}
        if "data" in envelope:
            out["data"] = body
        elif "event" in (envelope.get("type"), envelope.get("op")):
            out["payload"] = body
        else:
            raise
        return out


def encode_frame(envelope: dict) -> tuple[bytes, bool]:
    """Serialize an envelope for the wire. Returns (wire_bytes, is_binary). If any
    bytes value is present, a binary frame `[4-byte BE len | header JSON | body]`;
    otherwise UTF-8 JSON. (Even the text case returns bytes — a WS transport decodes
    it back to a `str` for a text frame; a byte-stream transport sends it as-is.)"""
    found = find_bytes_path(envelope)
    if found is None:
        return json.dumps(envelope, default=str).encode("utf-8"), False
    _one_body(envelope)
    path, body = found
    head_obj = copy.deepcopy(envelope)
    set_path(head_obj, path, None)
    head_obj["_binary_path"] = path
    head_bytes = json.dumps(head_obj, default=str).encode("utf-8")
    return struct.pack(">I", len(head_bytes)) + head_bytes + body, True


_INT64 = (-(2**63), 2**63 - 1)


def _no_constant(name: str):
    raise ValueError(f"{name} is not JSON")


def _int64(digits: str) -> int:
    v = int(digits)
    if not _INT64[0] <= v <= _INT64[1]:
        raise ValueError(f"integer {digits} is outside int64")
    return v


def _finite(digits: str) -> float:
    v = float(digits)
    if not math.isfinite(v):
        raise ValueError(f"number {digits} overflows a double")
    return v


def _no_lone_surrogate(x) -> None:
    """A `\\ud800`–`\\udfff` escape with no partner decodes to a code point that is
    not a Unicode scalar: python holds it, UTF-8 cannot write it, rust strings
    cannot hold it at all."""
    if isinstance(x, str):
        if any(0xD800 <= ord(c) <= 0xDFFF for c in x):
            raise ValueError("a lone surrogate (\\ud800–\\udfff) is not text")
    elif isinstance(x, dict):
        for k, v in x.items():
            _no_lone_surrogate(k)
            _no_lone_surrogate(v)
    elif isinstance(x, list):
        for v in x:
            _no_lone_surrogate(v)


def loads(text: str | bytes):
    """JSON off the wire, STRICTLY — only values EVERY runtime can hold. Refused, each as a `JSONDecodeError` so every caller's
    existing "not valid JSON" path answers it:
      * `NaN`, `Infinity`, `-Infinity` — not JSON at all (RFC 8259), yet python's
        `json.loads` alone accepts them;
      * an integer outside int64, or a number that overflows a double — rust
        parses it into a different value;
      * a lone surrogate in a string — not text; no runtime can write it as UTF-8;
      * a document nested deeper than the json scanner can enter — it charges
        every `[`/`{` against `sys.getrecursionlimit()` and raises
        `RecursionError`, which no "not valid JSON" path catches (web_rest
        would answer a bare 500)."""
    try:
        out = json.loads(
            text, parse_constant=_no_constant, parse_int=_int64, parse_float=_finite
        )
        _no_lone_surrogate(out)
        return out
    except (ValueError, RecursionError) as e:
        if isinstance(e, json.JSONDecodeError):
            raise
        doc = text if isinstance(text, str) else ""
        raise json.JSONDecodeError(str(e), doc, 0) from None


def decode_frame(data: bytes | str) -> dict:
    """Parse a wire frame back into an envelope. `str` ⇒ a text (JSON) frame; `bytes`
    ⇒ a binary frame with the raw body restored at `_binary_path`.

    A frame arrives from a PEER, so every length in it is hostile until checked: a
    4-byte prefix claiming 4 GiB of header on a 3-byte frame would slice garbage
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
