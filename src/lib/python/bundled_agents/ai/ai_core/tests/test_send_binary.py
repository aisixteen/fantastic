"""`send_binary` — the `bytes` field carries a RAW program, read without json.loads.

The failure this removes, measured live (subai/subai_graph, 2026-08-30): a program in a
JSON string field must be perfectly escaped or `json.loads` on the whole tool_call
rejects it (`invalid JSON — Expecting ',' delimiter`), and a model can't escape a big,
quote-heavy program reliably. But the program is a STRING WE READ, not data we parse —
so for `send_binary` the `bytes` field is lifted out RAW: everything from `"bytes":"` to
the final `"` before the closing braces is taken verbatim (no unescaping), and only the
STRUCTURE around it is JSON-parsed. `bytes` must be the LAST field. Then ai_core
utf-8-encodes that raw string to real bytes and dispatches an ordinary send, so a stream
sink like `file_bridge.write_stream` (which refuses a str) gets its binary.

Two layers: `tool_parse._send_binary_raw` (raw extraction) and `core` (str→bytes encode).
"""

from __future__ import annotations

import asyncio

from ai_core.core import _route_reply
from ai_core.tool_parse import extract_tool_calls


def _binary_text(target: str, meta: dict, program: str) -> str:
    """A send_binary tool_call with `program` written RAW as the ABSOLUTE last field.

    Mirrors what the prompt teaches: `reply_to`/metadata first, then the payload ending
    in `"bytes":"<program verbatim>"` with nothing escaped — so nothing follows the
    program but closing braces."""
    import json

    fields = json.dumps(meta)[1:-1]  # payload's metadata fields, no braces
    prefix = fields + "," if fields else ""
    inner = (
        '{"name":"send_binary","arguments":{"target_id":'
        + json.dumps(target)
        + ',"reply_to":"self","payload":{'
        + prefix
        + '"bytes":"'
        + program
        + '"}}}'
    )
    return "<tool_call>" + inner + "</tool_call>"


# ── the parser: raw extraction, no escaping needed ──────────────────────────


def test_unescaped_program_parses_verbatim():
    prog = 'import os\nx = "a \\ b"\nprint("IMPORT_OK", os.listdir("."))\n'  # real \n + quotes
    text = _binary_text(
        "store", {"type": "write_stream", "path": "main.py", "truncate": True}, prog
    )
    calls = extract_tool_calls(text)
    assert len(calls) == 1
    c = calls[0]
    assert c["name"] == "send_binary"
    p = c["arguments"]["payload"]
    assert p["bytes"] == prog, "the program is taken verbatim — no unescaping"
    assert p["path"] == "main.py" and p["truncate"] is True


def test_program_with_json_metacharacters_survives():
    # braces, quotes, backslashes — the exact stuff that breaks json.loads
    prog = 'd = {"k": "v"}\nprint(f"{d}")\npath = "C:\\\\tmp"\n'
    text = _binary_text("store", {"type": "write_stream", "path": "x.py"}, prog)
    calls = extract_tool_calls(text)
    assert len(calls) == 1
    assert calls[0]["arguments"]["payload"]["bytes"] == prog


def test_not_send_binary_is_untouched():
    plain = (
        '<tool_call>{"name":"send","arguments":'
        '{"target_id":"log","payload":{"type":"note","text":"hi"}}}</tool_call>'
    )
    calls = extract_tool_calls(plain)
    assert len(calls) == 1 and calls[0]["name"] == "send"
    assert calls[0]["arguments"]["payload"]["text"] == "hi"


# ── the dispatch: raw str → real bytes ──────────────────────────────────────


class _Kernel:
    def __init__(self):
        self.dispatched: list[dict] = []

    def get(self, _id):
        return {}

    async def send(self, items):
        self.dispatched.extend(items)
        return [
            {"pid": f"{it['target']}:beef", "status": "completed", "result": {}}
            for it in items
        ]


async def _settle(k, want, timeout=2.0):
    began = asyncio.get_event_loop().time()
    while len(k.dispatched) < want:
        if asyncio.get_event_loop().time() - began > timeout:
            break
        await asyncio.sleep(0.005)


async def test_dispatch_encodes_the_raw_program_to_bytes():
    k = _Kernel()
    prog = 'print("hi")\nx = 1\n'
    text = _binary_text(
        "store", {"type": "write_stream", "path": "main.py", "truncate": True}, prog
    )
    await _route_reply("ai", k, text, "probe", None, None, 180.0, "")
    await _settle(k, 1)
    sent = [it for it in k.dispatched if it["payload"].get("type") == "write_stream"]
    assert len(sent) == 1
    payload = sent[0]["payload"]
    assert payload["bytes"] == prog.encode("utf-8"), "raw string encoded to real bytes"
    assert isinstance(payload["bytes"], bytes)
    assert payload["path"] == "main.py"
