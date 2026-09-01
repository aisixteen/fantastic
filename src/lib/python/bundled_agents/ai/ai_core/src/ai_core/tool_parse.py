"""Tool-call parsing — the text shapes a model may use to name a `send`.

TWO callers, and the first of them is LIVE (this docstring used to say the
parser was "retained ONLY for the context-reaction read-model"; the routing work
of 2026-08-02 made it load-bearing again):

  - `core._route_reply` — every completion is scanned here, and each call found is
    dispatched with its reply delivered to its own `reply_to`. This is the path a
    chain continues on. A shape this module does not recognize is a turn that
    silently does nothing.
  - `core._derive_reaction` — scans the DURABLE transcript for whether the model
    reacted to a compaction notice (e.g. by emitting a `recall`/persist call).

There is still NO agentic loop and NO native provider tool API: the provider
streams plain text, this module reads it. Three envelopes are recognized, because
a model's trained format beats our instruction and refusing a shape costs a whole
run (each is documented at its own constants below):

    <tool_call>{"name":"send","arguments":{…}}</tool_call>     taught (Hermes-style)
    <invoke name="send"><parameter …>…</parameter></invoke>    Claude's own
    <|tool_call>call:<name>{…}<tool_call|>                     special-token channel
"""

from __future__ import annotations

import json
import re
import secrets

OPEN = "<tool_call>"
CLOSE = "</tool_call>"

# A SECOND text envelope, because a model's trained format beats our instruction.
# Claude models routinely emit their own shape instead of the taught one:
#
#   <invoke name="send">
#     <parameter name="arguments">{"target_id":"notes", …, "reply_to":"self"}</parameter>
#   </invoke>
#
# with the arguments perfectly correct — only the wrapper differs. Refusing it dropped
# whole runs at pass 1 (observed on claude-haiku-4.5), after which the model, never
# receiving a reply, invented the entire remaining session.
#
# This is STILL raw prompt-and-parse: it is plain text the model typed. There is no
# `tools=[]`, no `tool_choice`, no native `tool_use` block — the provider is untouched
# and knows nothing. Parsing more text shapes is exactly this layer's job.
INVOKE_OPEN = "<invoke"
INVOKE_CLOSE = "</invoke>"
_INVOKE_NAME = re.compile(r'name\s*=\s*"([^"]*)"')
_INVOKE_PARAM = re.compile(
    r'<parameter\s+name\s*=\s*"([^"]*)"\s*>(.*?)</parameter>', re.DOTALL
)


def parse_invoke(block: str) -> dict | None:
    """Parse one `<invoke name="…">…</invoke>` body into `{id, name, arguments}`."""
    head, _, body = block.partition(">")
    m = _INVOKE_NAME.search(head)
    name = (m.group(1) if m else "") or "send"
    params = {k: v.strip() for k, v in _INVOKE_PARAM.findall(body)}
    if not params:
        return None
    args: dict = {}
    blob = params.pop("arguments", None)
    if blob is not None:
        try:
            parsed = json.loads(blob)
            if isinstance(parsed, dict):
                args = parsed
        except (json.JSONDecodeError, ValueError):
            pass
    # Remaining `<parameter name="x">` pairs are arguments too (models split them
    # either way). JSON-decode each value so objects/ints survive; else keep the text.
    for k, v in params.items():
        try:
            args[k] = json.loads(v)
        except (json.JSONDecodeError, ValueError):
            args[k] = v
    if not args:
        return None
    return {"id": _mint_id(), "name": str(name), "arguments": args}


# A THIRD text envelope, from the SPECIAL-TOKEN family. A `|` inside the brackets
# marks a token that lives in the tokenizer's VOCABULARY rather than in ordinary
# text, so the delimiter cannot be forged by the content it wraps — the same idea as
# ChatML's `<|im_start|>`, Llama-3's `<|eot_id|>` / `<|python_tag|>`, harmony's
# `<|channel|>`, and DeepSeek's tool-call markers. What distinguishes THIS grammar is
# that the pipe MIRRORS across the pair — `<|tool_call>` opens, `<tool_call|>` closes
# — and that strings carry a quoting token of their own, `<|"|>`, instead of `"`:
#
#   <|tool_call>call:get_current_weather{location:<|"|>Tokyo, JP<|"|>}<tool_call|>
#
# So the argument body is NOT JSON. Keys are bare, strings wear `<|"|>`, and numbers,
# booleans, lists and nested objects are written bare. `json.loads` over that body
# succeeds ONLY for the empty `{}` — every call carrying an actual argument parses as
# malformed and is silently dropped, which is the worst possible failure here.
#
# Measured 2026-08-04 on gemma-4-12B-it: it planned correctly in its thought channel,
# chose the right agent AND verb, emitted
# `<|tool_call>call:kernel_state.list_agents{}<tool_call|>`, and the run died at pass
# 1 with zero calls extracted. `<tool_call>` is not a substring of either tag — the
# pipe breaks the match — so the taught envelope's finder never saw it at all.
#
# Two body shapes reach us, because a model compresses our ONE universal tool:
#   call:send{target_id:<|"|>x<|"|>,payload:{…}}   — our taught tool, in this syntax
#   call:<target>.<verb>{…}                        — the same thing, compressed
# The second maps onto `send(target_id=<target>, payload={"type": <verb>, **args})`.
# `reply_to` has no slot and needs none: it defaults to SELF, so the reply arrives as
# the next turn, which is what a loop wants.
#
# Still raw prompt-and-parse — plain text the model typed. No `tools=[]`, no
# `tool_choice`, no native tool_use block; the provider is untouched.
CHANNEL_OPEN = "<|tool_call>"
CHANNEL_CLOSE = "<tool_call|>"
CHANNEL_STR = '<|"|>'
_CHANNEL_HEAD = re.compile(r"\s*call\s*:\s*([^\s{]+)\s*")
# Sentinel: `None` is a legal VALUE, so it cannot double as the failure signal.
_BAD = object()


def _skip_ws(s: str, i: int) -> int:
    while i < len(s) and s[i].isspace():
        i += 1
    return i


# The escapes a quoted channel string may carry. Shared by both quote styles so
# `'a\nb'` and "a\nb" decode identically — a model should not get a different
# string back for picking a different quote character.
_ESCAPES = {
    '"': '"',
    "'": "'",
    "\\": "\\",
    "/": "/",
    "n": "\n",
    "r": "\r",
    "t": "\t",
    "b": "\b",
    "f": "\f",
}


def _parse_quoted(s: str, i: int):
    """(string, next_index) for a `'…'` or "…" value. `-1` means malformed.

    BOTH quote styles, because which one a model reaches for is a property of its
    training, not of our grammar — the documented gemma grammar uses `<|"|>`, the
    2026-08-04 run mixed in JSON double quotes, and the 2026-08-06 run emitted
    `call:kernel_state{type: 'send', target_id: 'kernel', …}` in single quotes. That
    last one was a fully correct intent and the parser threw it away, which ended
    the run: the model had nothing left to fix.

    ONLY the matching quote closes. So an opener of one style closed by the other
    never terminates, runs to the end of the body, and is reported malformed —
    which is right, because a mismatched pair is a real error and not a shape any
    model actually means."""
    q = s[i]
    out: list[str] = []
    i += 1
    while i < len(s):
        c = s[i]
        if c == "\\" and i + 1 < len(s):
            nxt = s[i + 1]
            if nxt == "u" and i + 5 < len(s):
                try:
                    out.append(chr(int(s[i + 2 : i + 6], 16)))
                    i += 6
                    continue
                except ValueError:
                    pass
            # An unknown escape keeps both characters rather than guessing: a
            # Windows path in a program body (`\d`) must survive verbatim.
            out.append(_ESCAPES.get(nxt, "\\" + nxt))
            i += 2
            continue
        if c == q:
            return "".join(out), i + 1
        out.append(c)
        i += 1
    return _BAD, -1


def _parse_channel_value(s: str, i: int):
    """(value, next_index). `next_index == -1` means malformed."""
    i = _skip_ws(s, i)
    if i >= len(s):
        return _BAD, -1
    if s.startswith(CHANNEL_STR, i):
        j = s.find(CHANNEL_STR, i + len(CHANNEL_STR))
        if j == -1:
            return _BAD, -1
        return s[i + len(CHANNEL_STR) : j], j + len(CHANNEL_STR)
    if s[i] in "\"'":
        # MIXED SYNTAX — the envelope is the channel one, the strings are ordinary
        # quoted text. Measured 2026-08-04: gemma-4-12B-it emitted
        #   call:kernel_state.update_agent{"id":"assets","ingress_rule":"allow_all_ingress"}
        # Without this branch the bare-token path returns `"assets"` WITH its quote
        # characters, `id` never arrives, and the call comes back
        # `{"error": "update_agent: id required"}` — which is what killed all three
        # posture changes in that run.
        return _parse_quoted(s, i)
    if s[i] == "{":
        return _parse_channel_object(s, i)
    if s[i] == "[":
        out: list = []
        i = _skip_ws(s, i + 1)
        if i < len(s) and s[i] == "]":
            return out, i + 1
        while True:
            v, i = _parse_channel_value(s, i)
            if i == -1:
                return _BAD, -1
            out.append(v)
            i = _skip_ws(s, i)
            if i < len(s) and s[i] == ",":
                i += 1
                continue
            if i < len(s) and s[i] == "]":
                return out, i + 1
            return _BAD, -1
    # A BARE token, ending at the first structural character.
    j = i
    while j < len(s) and s[j] not in ",}]":
        j += 1
    raw = s[i:j].strip()
    if not raw:
        return _BAD, -1
    low = raw.lower()
    if low == "true":
        return True, j
    if low == "false":
        return False, j
    if low in ("null", "none"):
        return None, j
    try:
        return int(raw), j
    except ValueError:
        pass
    try:
        return float(raw), j
    except ValueError:
        pass
    return raw, j


def _parse_channel_object(s: str, i: int):
    """(dict, next_index) for a `{k:v,k:v}` body. `next_index == -1` means malformed."""
    i = _skip_ws(s, i)
    if i >= len(s) or s[i] != "{":
        return _BAD, -1
    i = _skip_ws(s, i + 1)
    out: dict = {}
    if i < len(s) and s[i] == "}":
        return out, i + 1
    while True:
        i = _skip_ws(s, i)
        # Keys are bare in the documented grammar, but a model mixing JSON quotes
        # them — in EITHER style. Parsed with the same reader as a value so a key
        # is allowed to contain `:` or `,`; scanning bare to the first structural
        # character would split such a key in half and lose the rest.
        if i < len(s) and s[i] in "\"'":
            key, k = _parse_quoted(s, i)
            if k == -1:
                return _BAD, -1
            k = _skip_ws(s, k)
        else:
            k = i
            while k < len(s) and s[k] not in ":,}":
                k += 1
            key = s[i:k].strip()
        if not key or k >= len(s) or s[k] != ":":
            return _BAD, -1
        v, i = _parse_channel_value(s, k + 1)
        if i == -1:
            return _BAD, -1
        out[key] = v
        i = _skip_ws(s, i)
        if i < len(s) and s[i] == ",":
            i += 1
            continue
        if i < len(s) and s[i] == "}":
            return out, i + 1
        return _BAD, -1


def parse_channel_call(block: str) -> dict | None:
    """Parse one `call:<name>{…}` body into the universal `send` call."""
    m = _CHANNEL_HEAD.match(block)
    if m is None:
        return None
    name = m.group(1)
    args, end = _parse_channel_object(block, m.end())
    if end == -1 or not isinstance(args, dict):
        # Malformed args are NOT repaired — a parser that guesses what the model
        # meant is measuring its own guess. Drop the call; the model gets no reply
        # and can restate it.
        return None
    # THE ARGS DECIDE, not the name. A model that already wrote `target_id` has
    # written canonical `send` arguments, and any dotted prefix on the name is a
    # NAMESPACE it added, not a target. Measured 2026-08-04: gemma-4-12B-it emitted
    #   call:kernel_state.send{"payload":{"type":"reflect"},"target_id":"kernel_state"}
    # Reading the prefix as the target turned that into
    # `send(kernel_state, {"type":"send", …})` and came back
    # `{"error": "kernel_state: unknown type 'send'"}` eight times in one run.
    if "target_id" in args:
        if "payload" in args:
            return {"id": _mint_id(), "name": "send", "arguments": args}
        # target_id, no payload, but a `type` sitting among the args: the payload is
        # INLINE — the model flattened one level instead of nesting. Measured
        # 2026-08-06, gemma-4-12B emitted
        #   call:kernel_state{type: 'reflect', target_id: 'readings'}
        # meaning "reflect readings". Before this branch the name was not dotted, so
        # the verb was looked for only in the name, found nowhere, and a complete
        # instruction was discarded. `type` beats a dotted verb here for the reason
        # the block below already gives: THE ARGS DECIDE.
        if "type" in args:
            return _inline_send(args["target_id"], args)
        # target_id but NO payload: the VERB is still in the name and must not be
        # dropped. Measured 2026-08-04 — `call:kernel_state.reflect{"target_id":
        # "transcripts"}` means "reflect transcripts". An earlier revision of this
        # branch returned a send with no payload at all; `_route_reply` dispatched
        # `None` and the handler raised `'NoneType' object has no attribute 'get'`,
        # three times in one run.
        if "." in name:
            verb = name.partition(".")[2]
            rest = {k: v for k, v in args.items() if k != "target_id"}
            return {
                "id": _mint_id(),
                "name": "send",
                "arguments": {
                    "target_id": args["target_id"],
                    "payload": {"type": verb, **rest},
                },
            }
        # No payload and no verb anywhere — there is no call here to make.
        return None
    if "." in name:
        target, _, verb = name.partition(".")
        payload = {"type": verb}
        payload.update(args)
        return {
            "id": _mint_id(),
            "name": "send",
            "arguments": {"target_id": target, "payload": payload},
        }
    if not args:
        return None
    # No target_id and an undotted name, but a `type` among the args: then the NAME
    # is the agent and the args are its payload — `call:kernel_state{type:'reflect'}`
    # is `send('kernel_state', {'type':'reflect'})`. Without this the call kept the
    # agent's name in `name` where nothing reads it, arrived with no `target_id`, and
    # earned a `no target_id` error for a request that was perfectly clear.
    if "type" in args:
        return _inline_send(name, args)
    return {"id": _mint_id(), "name": name, "arguments": args}


def _inline_send(target, args: dict) -> dict | None:
    """A `send` whose payload was written FLAT among the args.

    `target_id` names the destination and `reply_to` says where the answer goes —
    both are routing, so they stay at the top level. Everything else is the payload.
    """
    if not isinstance(target, str) or not target.strip():
        return None
    out: dict = {
        "target_id": target.strip(),
        "payload": {
            k: v for k, v in args.items() if k not in ("target_id", "reply_to")
        },
    }
    if "reply_to" in args:
        out["reply_to"] = args["reply_to"]
    return {"id": _mint_id(), "name": "send", "arguments": out}


def _mint_id() -> str:
    return f"call_{secrets.token_hex(4)}"


_SB_BYTES = re.compile(r'"bytes"\s*:\s*"')


def _send_binary_raw(inner: str) -> dict | None:
    """`send_binary` whose `bytes` field is a program that must NOT be JSON-escaped.

    The whole reason a program-in-JSON fails is that `json.loads` demands every quote,
    newline and backslash be escaped, and a model can't do that reliably on a big
    program. But the PROGRAM is not data we need parsed — it is a string we read. So
    for `send_binary` we lift `bytes` out RAW: everything from `"bytes":"` to the final
    `"` in the envelope is the program, verbatim (no unescaping), and only the STRUCTURE
    around it is JSON-parsed. `bytes` must be the LAST field (nothing but closing braces
    after its value) so its end is unambiguous — that is the whole contract.
    """
    if '"send_binary"' not in inner:
        return None
    m = _SB_BYTES.search(inner)
    if not m:
        return None
    val_start = m.end()  # first char of the raw program
    val_end = inner.rfind('"')  # bytes is last, so its closing quote is the last quote
    if val_end <= val_start:
        return None
    raw = inner[val_start:val_end]  # the program, exactly as written
    # Re-close the JSON with an EMPTY bytes value → a well-formed object we can parse
    # for the structure (name, target_id, payload metadata) without touching `raw`.
    skeleton = inner[:val_start] + inner[val_end:]
    try:
        obj = json.loads(skeleton)
    except (json.JSONDecodeError, ValueError):
        return None
    if not isinstance(obj, dict) or obj.get("name") != "send_binary":
        return None
    args = obj.get("arguments")
    if not isinstance(args, dict):
        return None
    payload = args.get("payload")
    (payload if isinstance(payload, dict) else args)["bytes"] = raw
    return {"id": _mint_id(), "name": "send_binary", "arguments": args}


# ─── the FENCED form: the program rides OUTSIDE the JSON ────────────────────
#
# Measured 2026-09-01 (exercism gym, rust/word-count): the model followed the
# raw-tail contract exactly — real newlines, real apostrophes — and still lost
# the call to ONE missing closing brace, because the raw tail keeps the payload
# inside JSON syntax, so the model must still balance the skeleton by hand. The
# general fix removes closing syntax from the payload path entirely: the JSON
# stays small flat metadata (models don't break short JSON) with "@body" where
# the program goes, and the program rides verbatim between two column-0 marker
# lines. A marker at column 0 is never valid code at that position in any
# language, so there is nothing to escape, quote, or count — the design every
# surviving harness converged on (codex `*** Begin Patch`, aider fences).
FENCE_OPEN = "*** Begin Bytes"
FENCE_CLOSE = "*** End Bytes"
_FENCE_OPEN_RE = re.compile(r"^\*\*\* Begin Bytes[ \t]*\r?\n", re.M)
_FENCE_CLOSE_RE = re.compile(r"^\*\*\* End Bytes[ \t]*$", re.M)


def _split_fence(inner: str, m: re.Match) -> tuple[str, str]:
    """(metadata-JSON, raw program). The program runs from the line after
    `*** Begin Bytes` to the LAST column-0 `*** End Bytes` — last, so a program
    that somehow contains the closer is still taken whole. A missing closer
    takes the rest of the block: the envelope's own closing tag already bounds
    it, and dropping an unterminated program loses an unambiguous instruction
    (same stance as `_scan`'s unterminated-envelope tolerance)."""
    meta = inner[: m.start()]
    body_start = m.end()
    closes = list(_FENCE_CLOSE_RE.finditer(inner, body_start))
    raw = inner[body_start : closes[-1].start()] if closes else inner[body_start:]
    return meta, raw


def _plant_body(node, raw: str) -> bool:
    """Replace the first `"@body"` placeholder anywhere in the parsed call with
    the raw program. Returns whether one was found."""
    if isinstance(node, dict):
        for k, v in node.items():
            if v == "@body":
                node[k] = raw
                return True
            if _plant_body(v, raw):
                return True
    elif isinstance(node, list):
        for v in node:
            if _plant_body(v, raw):
                return True
    return False


def parse_one(inner: str) -> dict | None:
    """Parse the JSON between one tag pair into `{id, name, arguments}`.

    Lenient (tiny models drift): accepts `{"name","arguments"}`, a `tool` alias for
    `name`, a flattened object (remaining keys become arguments), and double-encoded
    (stringified) arguments. Returns None on unparseable JSON.
    """
    # FENCED form first (on the UNSTRIPPED block — a strip would eat the
    # program's trailing newline when the close marker was omitted): if the
    # markers are present, the JSON part is ONLY what sits above them — parsing
    # the whole block would either choke on the raw program or (when the program
    # happens to be valid JSON-adjacent) silently keep the "@body" placeholder
    # as the written contents.
    if (fm := _FENCE_OPEN_RE.search(inner)) is not None:
        meta, raw = _split_fence(inner, fm)
        call = parse_one(meta)
        if call is None:
            return None
        args = call.get("arguments")
        if isinstance(args, dict) and not _plant_body(args, raw):
            # No placeholder written — the program still has exactly one home.
            payload = args.get("payload")
            (payload if isinstance(payload, dict) else args)["bytes"] = raw
        return call
    inner = inner.strip()
    try:
        obj = json.loads(inner)
        rest = ""
    except (json.JSONDecodeError, ValueError):
        # send_binary carrying a RAW program in `bytes` (real newlines, real
        # quotes — no escaping) is not JSON, so it lands here; `_send_binary_raw`
        # lifts the program out verbatim. Tried ONLY after strict JSON fails:
        # a model that escaped its payload correctly (measured 2026-08-31, E44:
        # `"bytes":"<<<<<<< SEARCH\nport ..."`) parses above and gets its
        # escapes DECODED — the raw path would have kept them literal and the
        # patch died on "no blocks", twice, with no way for the model to see why.
        if (sb := _send_binary_raw(inner)) is not None:
            return sb
        # Trailing junk after a COMPLETE object — observed live from
        # gpt-oss:120b, which closes `arguments` early and then emits
        # `,"reply_to":"self"}}` (one brace too many). `raw_decode` takes the
        # object that IS there; the remainder is scanned below rather than
        # thrown away, because the model's real intent is usually in it.
        try:
            obj, end = json.JSONDecoder().raw_decode(inner)
            rest = inner[end:]
        except (json.JSONDecodeError, ValueError):
            return None
    if not isinstance(obj, dict):
        return None
    name = obj.get("name") or obj.get("tool") or "send"
    args = obj.get("arguments")
    if args is None:
        args = {k: v for k, v in obj.items() if k not in ("name", "tool", "arguments")}
    elif isinstance(args, dict):
        # Top-level keys that belong INSIDE arguments. Models routinely put
        # `reply_to` one level too high; `arguments` wins on conflict.
        strays = {
            k: v for k, v in obj.items() if k not in ("name", "tool", "arguments")
        }
        if rest:
            # `,"reply_to":"self"}}` → recover the k/v pairs the stray tail held.
            try:
                tail = json.loads("{" + rest.strip().lstrip(",").rstrip("}") + "}")
                if isinstance(tail, dict):
                    strays.update(tail)
            except (json.JSONDecodeError, ValueError):
                pass
        args = {**strays, **args}
    if isinstance(args, str):
        try:
            args = json.loads(args)
        except (json.JSONDecodeError, ValueError):
            args = {}
    if not isinstance(args, dict):
        args = {}
    name = str(name)
    # THE VERB IN THE TOOL NAME. `send` is the only tool there is, so a model that
    # writes any other name has compressed verb and tool into one — the same
    # compression `parse_channel_call` already handles for `call:<agent>.<verb>`,
    # arriving here in the JSON envelope instead. Measured 2026-08-06, gemma-4-12B:
    #   {"name":"reflect","arguments":{"target_id":"clock"}}
    # meaning "reflect the clock". Untranslated, `_route_reply` read `payload` as
    # absent and dispatched `send("clock", None)`, so the handler was called with
    # None and raised `'NoneType' object has no attribute 'get'` — three times in
    # the run this was found in. An explicit `type` in the args still wins, since
    # the args decide.
    if name != "send" and "payload" not in args and "target_id" in args:
        inline = _inline_send(args["target_id"], {"type": name, **args})
        if inline is not None:
            return inline
    return {"id": _mint_id(), "name": name, "arguments": args}


def _scan(text: str) -> list[tuple[str, str, dict | None, bool]]:
    """Every envelope found, in document order, as
    `(envelope, block, call|None, terminated)`.

    A `None` call means an envelope WAS opened and its body did not parse. The
    scanner keeps those instead of dropping them so a caller can tell the two
    outcomes apart — see `malformed_calls`. `terminated` says whether the closing
    tag was present, which is what separates a real attempt from a model merely
    MENTIONING the tag in prose. Splitting the scan out of `extract_tool_calls`
    keeps ONE implementation of the envelope precedence rules; two copies would
    drift the moment a fourth envelope arrives."""
    out: list[tuple[str, str, dict | None, bool]] = []
    i = 0
    while i < len(text):
        # Whichever envelope comes first, in document order. Channel before invoke
        # before taught on a tie, preserving the original precedence.
        found = [
            (pos, opener)
            for pos, opener in (
                (text.find(CHANNEL_OPEN, i), CHANNEL_OPEN),
                (text.find(INVOKE_OPEN, i), INVOKE_OPEN),
                (text.find(OPEN, i), OPEN),
            )
            if pos != -1
        ]
        if not found:
            break
        pos, opener = min(found, key=lambda f: f[0])
        closer, parser = _ENVELOPES[opener]
        end = text.find(closer, pos + len(opener))
        # UNTERMINATED (`end == -1`) is not a truncation guess: the envelope is the
        # LAST thing in the completion, so there is nothing after it to misread.
        # Observed live from nvidia/nemotron-3-ultra-550b, which emits a complete,
        # valid call object and simply stops without the closing tag. Dropping it
        # loses an unambiguous instruction.
        block = text[pos + len(opener) : end if end != -1 else len(text)]
        call = parser(block)
        if call is None and not _looks_like_an_attempt(opener, block):
            # A MENTION, not a call — the model wrote the tag inside a sentence.
            # Step over the OPENER ONLY. Consuming through `closer` would swallow
            # whatever follows, and what follows is often the real call: a stray
            # opener in prose pairs with the closing tag of the genuine call
            # further down, and the genuine call is then never seen. That silently
            # cost real instructions long before malformed reporting existed.
            i = pos + len(opener)
            continue
        out.append((opener, block, call, end != -1))
        if end == -1:
            break
        i = end + len(closer)
    return out


def extract_tool_calls(text: str) -> list[dict]:
    """Non-streaming: pull every finalized `<tool_call>` out of a complete string.
    Used by the durable-history reader (`core._derive_reaction`)."""
    return [call for _, _, call, _t in _scan(text) if call is not None]


# The advice appended to a malformed-envelope error. Carrying a program inside a
# JSON string is THE failure mode measured on gemma-4-12B (2026-08-06). Note what
# it actually got wrong, because it is the reverse of the obvious guess: the body
# was escaped CORRECTLY throughout, and the closing quote of the `code` value was
# simply never written —
#
#   …"code":"def update_totals():\n … return f\"…to {worker_res}\"}}
#                ^ opens here                                    ^ still open
#
# so every following brace was read as string content and the object ended
# unterminated. A model that escapes diligently can still lose the whole call by
# escaping the ONE quote that was supposed to close the value. Naming the rule is
# cheaper than letting it rediscover it, and this is advice, not a repair: we do
# not re-quote the body and retry.
_JSON_HINT = (
    "Your program broke its own JSON envelope. Do NOT debug the escaping — "
    "switch to the unbreakable FENCED form: keep the call's JSON small and "
    'complete with "@body" where the contents go, then write the program '
    "verbatim between two column-0 marker lines, still inside the same "
    "<tool_call>:\n"
    '{"name":"send_binary","arguments":{"target_id":"<id>","payload":'
    '{…,"bytes":"@body"}}}\n'
    "*** Begin Bytes\n"
    "<the program, verbatim — real newlines, real quotes, nothing escaped>\n"
    "*** End Bytes"
)

# The `<|tool_call>` body is NOT JSON, so the advice above would be actively
# misleading there — it was, on 2026-08-06, when a single-quoted channel call was
# answered with a lecture about escaping JSON. Advice has to match the grammar the
# model actually used, or it teaches the wrong lesson.
_CHANNEL_HINT = (
    "The `<|tool_call>` body is NOT JSON: write it as "
    "`call:<agent>{key: value, …}`. Strings may use ' or \" as long as the "
    "SAME character closes them, and every { [ ( must be closed too."
)

_HINTS = {OPEN: _JSON_HINT, CHANNEL_OPEN: _CHANNEL_HINT, INVOKE_OPEN: _JSON_HINT}


# opener -> (closer, body parser). One table, so the scanner has no per-envelope
# branches to keep in step and a fourth envelope is one entry.
_ENVELOPES = {
    OPEN: (CLOSE, parse_one),
    INVOKE_OPEN: (INVOKE_CLOSE, parse_invoke),
    CHANNEL_OPEN: (CHANNEL_CLOSE, parse_channel_call),
}


def _defang(text: str) -> str:
    """Neutralize envelope delimiters so quoted text cannot re-arm the scanner.

    Anything we echo back becomes conversation the model reads, quotes and reasons
    about — and a quoted `<tool_call>` is indistinguishable from an emitted one.
    Measured 2026-08-06: echoing a block verbatim made this error self-amplifying,
    each report seeding the next. Breaking the opener is enough; the text stays
    perfectly legible, which is the point of showing it at all."""
    for tag in (OPEN, CLOSE, CHANNEL_OPEN, CHANNEL_CLOSE, INVOKE_OPEN, INVOKE_CLOSE):
        text = text.replace(tag, tag.replace("<", "‹").replace(">", "›"))
    return text


def _looks_like_an_attempt(envelope: str, block: str) -> bool:
    """Did the model MEAN this opener as a call? Only asked of UNTERMINATED
    blocks, where the tag may just be a word in a sentence."""
    body = block.strip()
    if envelope == CHANNEL_OPEN:
        return bool(_CHANNEL_HEAD.match(body))
    if envelope == INVOKE_OPEN:
        return body.startswith("name=") or "<parameter" in body
    return body.startswith("{")


def malformed_calls(text: str) -> list[dict]:
    """Envelopes that opened and did not parse, as `{envelope, reason, excerpt}`.

    Exists because a call that cannot be read is the one failure a model cannot
    debug: `extract_tool_calls` returns `[]` for it, which is byte-identical to
    a turn that emitted no call at all — and that means "done". Measured on
    gemma-4-12B (2026-08-06): its last pass left the `code` string unclosed,
    nothing parsed, nothing dispatched, no error came back, and the run ended
    looking finished with the task untouched. Five earlier `no target_id` errors
    had TAUGHT it — it self-corrected out loud — so the fix is not a better
    parser, it is giving this failure the same voice the others already have."""
    out: list[dict] = []
    for envelope, block, call, terminated in _scan(text):
        if call is not None:
            continue
        if not _looks_like_an_attempt(envelope, block):
            # An opener whose body is not call-shaped is the model TALKING about the
            # envelope, not using it. Reporting those cost gemma-4-12B a turn on
            # 2026-08-06: it wrote the tag inside a sentence and got an error about
            # JSON.
            #
            # `terminated` is NOT the test, though the first version of this guard
            # used it. A stray opener in prose plus the closing tag of a REAL call
            # further down the completion makes a "terminated" block out of a
            # sentence, so requiring the closer let prose through — and then the
            # loop closed: this error echoes `your_block`, the model quotes the
            # error while reasoning about it, its quotation contains the tag, and
            # the next scan finds a fresh opener. Three of run 4's errors were that
            # loop feeding itself, not the model. Shape decides, always.
            continue
        reason = "the body did not parse"
        if envelope == OPEN:
            stripped = block.strip()
            # Diagnose the grammar the model ACTUALLY used — the strict-JSON
            # error for a raw/fenced attempt sends it debugging the wrong thing
            # (measured 2026-09-01: a raw-form call one brace short was told
            # "Invalid control character", reasoned about newlines for four
            # turns, and the write never landed).
            if (fm := _FENCE_OPEN_RE.search(stripped)) is not None:
                try:
                    json.loads(stripped[: fm.start()].strip())
                    reason = "the fenced call did not resolve"
                except (json.JSONDecodeError, ValueError) as exc:
                    reason = (
                        f"the metadata JSON above `{FENCE_OPEN}` did not parse — "
                        f"{exc}. It must be ONE complete, small JSON call object "
                        '(every brace closed) with "@body" where the program goes; '
                        "the program itself is fine, it rides verbatim between the "
                        "markers"
                    )
            elif '"send_binary"' in stripped and _SB_BYTES.search(stripped):
                reason = (
                    "the send_binary raw lift failed — the JSON metadata around "
                    "`bytes` must be valid and after the program's closing quote "
                    "nothing may follow but the braces that close that metadata "
                    "EXACTLY (count them)"
                )
            else:
                try:
                    json.loads(stripped)
                except (json.JSONDecodeError, ValueError) as exc:
                    reason = f"invalid JSON — {exc}"
        out.append(
            {
                # The whole reply body, composed HERE so the wording has one
                # owner. `core` delivers this verbatim; it does not assemble a
                # second version of the same sentence.
                "error": (
                    f"tool_call NOT dispatched: {reason}. Nothing ran. "
                    + _HINTS[envelope]
                ),
                "envelope": envelope,
                # BOTH ENDS, never just the head. A flat 400-char cap hid the
                # fault: the break in a long payload is almost never in the first
                # 400 — an unterminated string or a bad escape sits at the END. A
                # `create_agent` carrying 1.5k of `code` showed the model a prefix
                # that looked fine, so it could not see what to fix and re-sent the
                # same broken envelope. Measured 2026-08-24 (`live_build`): three
                # identical unparseable retries, run ended by the tripwire with the
                # artifact built but never run.
                #
                # A real program is now shown WHOLE; only a runaway is elided, and
                # then from the MIDDLE so the tail survives. Bounded still, because
                # a 40k body would otherwise blow the context it is being explained
                # in. DEFANGED so quoting it cannot arm the scanner again.
                "your_block": _defang(block.strip()),
            }
        )
    return out
