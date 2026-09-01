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


def test_escaped_json_send_binary_decodes_its_escapes():
    """A model that serializes the WHOLE call as strict JSON (escaped \\n in
    `bytes`) must get real newlines — strict parse wins over the raw-tail path.
    Measured 2026-08-31 (E44): the raw path kept the escapes literal and a
    valid patch died on 'no blocks'."""
    import json as _json

    from ai_core.tool_parse import extract_tool_calls

    inner = _json.dumps(
        {
            "name": "send_binary",
            "arguments": {
                "target_id": "editor",
                "payload": {
                    "type": "patch",
                    "edits": [{"path": "config.txt", "hunks": 1}],
                    "checkpoint": True,
                    "bytes": "<<<<<<< SEARCH\nport = 8080\n=======\nport = 9090\n>>>>>>> REPLACE\n",
                },
            },
        }
    )
    calls = extract_tool_calls("<tool_call>" + inner + "</tool_call>")
    assert len(calls) == 1
    b = calls[0]["arguments"]["payload"]["bytes"]
    assert "\n" in b and "\\n" not in b
    assert b.startswith("<<<<<<< SEARCH\nport = 8080")


def test_raw_unescaped_send_binary_still_rides_verbatim():
    """The raw-tail contract is untouched: real newlines/quotes in `bytes`
    (not valid JSON) still lift out verbatim."""
    from ai_core.tool_parse import extract_tool_calls

    inner = (
        '{"name":"send_binary","arguments":{"target_id":"editor","payload":'
        '{"type":"patch","path":"m.py","bytes":"line "one"\nline two"}}}'
    )
    calls = extract_tool_calls("<tool_call>" + inner + "</tool_call>")
    assert len(calls) == 1
    assert calls[0]["arguments"]["payload"]["bytes"] == 'line "one"\nline two'


# ── the FENCED form: the program rides OUTSIDE the JSON ─────────────────────


RUST = (
    "use std::collections::HashMap;\n\n"
    "pub fn word_count(words: &str) -> HashMap<String, u32> {\n"
    "    let mut counts: HashMap<String, u32> = HashMap::new();\n"
    "    for token in words.split(|c: char| !c.is_alphanumeric() && c != ''') {\n"
    "        let word = token.trim_matches(''');\n"
    "        if !word.is_empty() {\n"
    "            *counts.entry(word.to_lowercase()).or_insert(0) += 1;\n"
    "        }\n"
    "    }\n"
    "    counts\n"
    "}\n"
)


def _fenced_text(meta_json: str, program: str, close: bool = True) -> str:
    tail = "*** End Bytes\n" if close else ""
    return (
        "<tool_call>"
        + meta_json
        + "\n*** Begin Bytes\n"
        + program
        + tail
        + "</tool_call>"
    )


def test_fenced_program_rides_verbatim():
    """The gym's killer case: quote-heavy rust with apostrophes and braces —
    verbatim through the fence, no escaping, no skeleton to balance."""
    meta = (
        '{"name":"send_binary","arguments":{"target_id":"editor","payload":'
        '{"type":"write","path":"src/lib.rs","truncate":true,"bytes":"@body"}}}'
    )
    calls = extract_tool_calls(_fenced_text(meta, RUST))
    assert len(calls) == 1
    p = calls[0]["arguments"]["payload"]
    assert p["bytes"] == RUST
    assert p["path"] == "src/lib.rs" and p["truncate"] is True


def test_fenced_without_placeholder_lands_in_payload_bytes():
    meta = (
        '{"name":"send_binary","arguments":{"target_id":"editor","payload":'
        '{"type":"write","path":"m.py"}}}'
    )
    calls = extract_tool_calls(_fenced_text(meta, "x = 1\n"))
    assert len(calls) == 1
    assert calls[0]["arguments"]["payload"]["bytes"] == "x = 1\n"


def test_fenced_missing_close_marker_takes_rest_of_block():
    meta = (
        '{"name":"send_binary","arguments":{"target_id":"editor","payload":'
        '{"type":"write","path":"m.py","bytes":"@body"}}}'
    )
    calls = extract_tool_calls(_fenced_text(meta, "a = 1\nb = 2\n", close=False))
    assert len(calls) == 1
    assert calls[0]["arguments"]["payload"]["bytes"] == "a = 1\nb = 2\n"


def test_fenced_bad_metadata_gets_a_fence_error_not_a_json_red_herring():
    from ai_core.tool_parse import malformed_calls

    meta = (  # one closing brace SHORT — the measured live failure
        '{"name":"send_binary","arguments":{"target_id":"editor","payload":'
        '{"type":"write","path":"m.py","bytes":"@body"}}'
    )
    text = _fenced_text(meta, "x = 1\n")
    assert extract_tool_calls(text) == []
    errs = malformed_calls(text)
    assert len(errs) == 1
    assert "Begin Bytes" in errs[0]["error"], "advice names the fence grammar"
    assert "Invalid control character" not in errs[0]["error"]


def test_raw_form_one_brace_short_gets_truthful_advice():
    """The exact turn-5 failure from the gym: raw form, skeleton one `}` short.
    The error must name the raw-lift fault, never the strict-JSON red herring."""
    from ai_core.tool_parse import malformed_calls

    inner = (
        '{"name":"send_binary","arguments":{"target_id":"editor","payload":'
        '{"type":"write","path":"src/lib.rs","truncate":true,"bytes":"' + RUST + '"}}'
    )  # two closes after the quote; three needed
    text = "<tool_call>" + inner + "</tool_call>"
    assert extract_tool_calls(text) == []
    errs = malformed_calls(text)
    assert len(errs) == 1
    assert "raw lift" in errs[0]["error"]
    assert "Invalid control character" not in errs[0]["error"]


async def test_fenced_dispatch_encodes_to_bytes():
    k = _Kernel()
    meta = (
        '{"name":"send_binary","arguments":{"target_id":"store","payload":'
        '{"type":"write_stream","path":"main.py","truncate":true,"bytes":"@body"}}}'
    )
    await _route_reply(
        "ai", k, _fenced_text(meta, 'print("hi")\n'), "probe", None, None, 180.0, ""
    )
    await _settle(k, 1)
    sent = [it for it in k.dispatched if it["payload"].get("type") == "write_stream"]
    assert len(sent) == 1
    assert sent[0]["payload"]["bytes"] == b'print("hi")\n'


async def test_literal_body_placeholder_is_refused_not_written():
    """`bytes:"@body"` with NO fenced block is valid JSON — without the guard the
    literal placeholder became the file (measured 2026-09-01, gym: `illegal
    character '@'`). It must be refused loud, nothing dispatched."""
    k = _Kernel()
    text = (
        '<tool_call>{"name":"send_binary","arguments":{"target_id":"store",'
        '"reply_to":"self","payload":{"type":"write_stream","path":"main.py",'
        '"truncate":true,"bytes":"@body"}}}</tool_call>'
    )
    await _route_reply("ai", k, text, "probe", None, None, 180.0, "")
    await asyncio.sleep(0.05)
    sent = [it for it in k.dispatched if it["payload"].get("type") == "write_stream"]
    assert sent == [], "the placeholder must never reach the sink"
