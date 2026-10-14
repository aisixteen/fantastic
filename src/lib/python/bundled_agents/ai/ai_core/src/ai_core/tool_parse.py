"""Tool-call parsing — the ONE text shape a model uses to name a call.

TWO callers, both live:

  - `core._route_reply` — every completion is scanned here, and each call found is
    dispatched with its reply delivered to its own `reply_to`. A shape this module
    does not recognize is a turn that silently does nothing — so the unrecognized
    shape is reported back (`malformed_calls`), never guessed at.
  - `core._derive_reaction` — scans the DURABLE transcript for whether the model
    reacted to a compaction notice (e.g. by emitting a `recall`/persist call).

There is NO agentic loop and NO native provider tool API: the provider streams plain
text, this module reads it. There is also NO JSON anywhere on this path. The grammar
is tag-delimited, args INLINE:

    <call name="send">
    <target_id>notes</target_id>
    <reply_to>self</reply_to>
    <type>patch</type>
    <edits><item><path>hello.py</path><hunks>2</hunks></item></edits>
    <bytes>…verbatim: real newlines, real quotes, ends at the first </bytes>…
    </bytes>
    </call>

Why tags and not JSON: JSON with a verbatim `bytes` cut goes wrong — escaped quotes,
literal `\\n`, a `\\"}` inside content ending the value early; bare child tags need
zero escapes, and nested lists parse every time. A `"` boundary inside
JSON fires the model's escaping reflex; a tag boundary carries none. Every harness that
carries documents in prompt-parsed calls moved them out of JSON quotes (aider text
blocks, OpenHands/cline per-parameter tags, codex freeform grammar).

Rules (all of them):
- Envelope `<call name="<tool>">…</call>`. Tools: `send`, `done`, `await`, `inquiry_answer`.
- `send`: `target_id` (or `to`, a list of recipients — one copy each), `re`,
  `reply_to`, `title`, `timeout`, `retries`, `exec_id` are CALL-level and stay there; every other child IS the payload (`type` names the verb). Other tools: the children
  are the arguments.
- An element with child elements → dict; children all named `<item>` → list;
  text → scalar typed by the ONE rule shared with the CLI (`kernel.modes.parse.
  coerce_scalar`: true/false, int, float, else str); an empty element → "".
- `bytes` is VERBATIM: never trimmed, never inspected, ends at the FIRST `</bytes>`,
  delivered as utf-8 bytes. While inside `<bytes>` the scanner is in verbatim mode, so
  a `</call>` in a document does not end the call.
- Nothing is repaired. A call that does not parse is reported with its fault named and
  the minimal shape to send; the model fixes it.
"""

from __future__ import annotations

import re
import secrets

from kernel.modes.parse import coerce_scalar

OPEN = "<call"
CLOSE = "</call>"
BYTES_OPEN = "<bytes>"
BYTES_CLOSE = "</bytes>"

_HEAD = re.compile(r'^\s*name\s*=\s*"([A-Za-z_][\w]*)"\s*>', re.DOTALL)
# A tag name is a KEY, not an XML name: grant maps are keyed by agent ids and the
# relative tokens (`$self`, `$parent`, `$subtree`), and the model writes those as
# tags (`<$self>*</$self>`) — so `$`,
# `-` and `.` are letters here.
_TAG = re.compile(r"<([A-Za-z_$][\w$.\-]*)>")
# Call-level fields of a `send`: where it goes, where the answer goes, what it is for,
# and the kernel's per-item bounds. Everything else in the call is the payload. The
# kernel item's caller-chosen execution name is `exec_id` HERE (core maps it onto the
# item's `id`): a bare `<id>` would collide with the many verbs whose payload has an
# `id` (`delete_agent`, `update_agent`, …) now that arguments are inline.
_ROUTING = (
    "target_id",
    "to",
    "re",
    "reply_to",
    "title",
    "timeout",
    "retries",
    "exec_id",
)
# The tools. Any other `name` is refused with the shape — a verb is `<type>`, never
# the tool name (a compression small models made under the JSON grammar).
TOOLS = ("send", "done", "await", "inquiry_answer")


class _Malformed(Exception):
    """The block is a call the model MEANT and it does not parse; `str(exc)` is the
    fault, named for the model."""


def _find_close(text: str, start: int) -> int:
    """Index of the `</call>` that closes a call whose body starts at `start`,
    skipping any `</call>` inside a `<bytes>` value; -1 if unterminated."""
    i = start
    while True:
        c = text.find(CLOSE, i)
        b = text.find(BYTES_OPEN, i)
        if b == -1 or (c != -1 and c < b):
            return c
        e = text.find(BYTES_CLOSE, b + len(BYTES_OPEN))
        if e == -1:
            return -1  # an open document runs to the end: unterminated
        i = e + len(BYTES_CLOSE)


def _elements(body: str) -> list[tuple[str, str]]:
    """`(name, raw)` for every child element of `body`, in order. Text between
    elements must be whitespace. `bytes` runs verbatim to its first closer."""
    out: list[tuple[str, str]] = []
    pos = 0
    while True:
        m = _TAG.search(body, pos)
        if m is None:
            tail = body[pos:]
            break
        between = body[pos : m.start()]
        if between.strip():
            raise _Malformed(
                f"text outside a tag (`{between.strip()[:40]}`) — every argument "
                "is its own `<name>…</name>` element"
            )
        name = m.group(1)
        closer = f"</{name}>"
        if name == "bytes":
            end = body.find(closer, m.end())
            if end == -1:
                raise _Malformed(
                    "`<bytes>` opened and never closed — close the document with "
                    "`</bytes>`, then the call with `</call>`"
                )
            out.append((name, body[m.end() : end]))
            pos = end + len(closer)
            continue
        end = _matching_close(body, name, m.end())
        if end == -1:
            raise _Malformed(f"`<{name}>` opened and never closed with `{closer}`")
        out.append((name, body[m.end() : end]))
        pos = end + len(closer)
    if tail.strip():
        raise _Malformed(
            f"text after the last tag (`{tail.strip()[:40]}`) — every argument is "
            "its own `<name>…</name>` element"
        )
    return out


def _matching_close(body: str, name: str, start: int) -> int:
    """The closer of `<name>` opened just before `start`, honouring nesting of the
    SAME name and skipping `<bytes>` values inside."""
    opener, closer = f"<{name}>", f"</{name}>"
    depth, i = 1, start
    while depth:
        c = body.find(closer, i)
        if c == -1:
            return -1
        o = body.find(opener, i)
        b = body.find(BYTES_OPEN, i)
        nxt = min(x for x in (c, o, b) if x != -1)
        if nxt == b:
            e = body.find(BYTES_CLOSE, b + len(BYTES_OPEN))
            if e == -1:
                return -1
            i = e + len(BYTES_CLOSE)
        elif nxt == o:
            depth += 1
            i = o + len(opener)
        else:
            depth -= 1
            if depth == 0:
                return c
            i = c + len(closer)
    return -1


def _value(name: str, raw: str):
    """One element's value: verbatim bytes, a nested dict/list, or a typed scalar."""
    if name == "bytes":
        return raw.encode("utf-8")
    if _TAG.search(raw) is None or not raw.strip().startswith("<"):
        return coerce_scalar(raw.strip())
    kids = _elements(raw)
    if not kids:
        return coerce_scalar(raw.strip())
    names = [k for k, _ in kids]
    # A LIST is a tag whose children are all `<item>` — one element each, any
    # count, one element included. The same tag the reply notation renders, so
    # what the model reads is what it writes (a reply showing
    # `<allowed><item>…</item></allowed>` is written back as-is, and a lone child
    # is still a one-element list).
    if all(k == "item" for k in names):
        return [_value(k, v) for k, v in kids]
    # `<item>` beside OTHER tags is an ordinary field named item
    # (`<payload><type>buy</type><item>rocket</item></payload>`). Only a tag whose children
    # are ALL `<item>` is a list; a one-field object named `item` is the single
    # shape the notation cannot say.
    out: dict = {}
    for k, v in kids:
        if k in out:
            raise _Malformed(
                f"duplicate `<{k}>` — a list is one tag whose children are all "
                f"`<item>`: `<{name}><item>…</item><item>…</item></{name}>`"
            )
        out[k] = _value(k, v)
    return out


def parse_call(block: str) -> dict | None:
    """Parse the text between `<call` and `</call>` into `{id, name, arguments}`.
    Raises `_Malformed` (fault named) for a call-shaped block that does not parse;
    returns None only for a block that is not a call at all (no `name="…">` head)."""
    m = _HEAD.match(block)
    if m is None:
        head = block.split(">", 1)[0].strip()
        if not head.startswith("name="):
            return None
        raise _Malformed(
            'the call head must be exactly `name="<tool>"` — nothing else before `>`'
        )
    name = m.group(1)
    if name not in TOOLS:
        raise _Malformed(
            f"`{name}` is not a tool — the tools are {', '.join(TOOLS)}. A VERB goes in "
            f'`<type>`: `<call name="send"><target_id>…</target_id><type>{name}</type></call>`'
        )
    body = block[m.end() :]
    fields: dict = {}
    for k, v in _elements(body):
        if k in fields:
            raise _Malformed(
                f"duplicate `<{k}>` — a list is one tag whose children are all "
                f"`<item>`: `<{k}><item>…</item><item>…</item></{k}>`"
            )
        fields[k] = _value(k, v)
    if name != "send":
        return {"id": _mint_id(), "name": name, "arguments": fields}
    target = fields.pop("target_id", None)
    to = fields.pop("to", None)
    if isinstance(to, str) and to.strip():
        to = [to.strip()]
    if isinstance(to, list):
        to = [str(t).strip() for t in to if str(t).strip()]
    if (not isinstance(target, str) or not target.strip()) and not to:
        raise _Malformed(
            "a send names its target: `<target_id>agent id</target_id>`, or several "
            "recipients as `<to><item>id</item><item>id</item></to>`"
        )
    args: dict = {}
    if isinstance(target, str) and target.strip():
        args["target_id"] = target.strip()
    if to:
        args["to"] = to
    for key in _ROUTING[2:]:
        if key in fields:
            args[key] = fields.pop(key)
    args["payload"] = fields
    return {"id": _mint_id(), "name": "send", "arguments": args}


def render_call(name: str, arguments: dict | None = None) -> str:
    """The inverse of `parse_call`: `{name, arguments}` as the ONE grammar's text.
    `send` arguments are the parser's own shape (`target_id`, `reply_to`?, `title`?,
    `payload`) and come out inline. A list renders as its outer tag with one child per
    element named by the outer's singular; `bytes` renders verbatim. Tests build
    fixtures with this so they cannot drift from the grammar; a renderer is also what
    anything that SHOWS a call to a human uses."""
    args = dict(arguments or {})
    if name == "send":
        payload = args.pop("payload", None) or {}
        flat = {k: args[k] for k in _ROUTING if k in args}
        flat.update(payload)
        args = flat
    body = "".join(_render_field(k, v) for k, v in args.items())
    return f'<call name="{name}">\n{body}</call>'


def _render_field(name: str, value) -> str:
    if name == "bytes" or isinstance(value, bytes):
        text = value.decode("utf-8") if isinstance(value, bytes) else str(value)
        return f"<bytes>{text}</bytes>\n"
    if isinstance(value, dict):
        inner = "".join(_render_field(k, v) for k, v in value.items())
        return f"<{name}>{inner}</{name}>\n"
    if isinstance(value, list):
        inner = "".join(_render_field("item", v) for v in value)
        return f"<{name}>{inner}</{name}>\n"
    if isinstance(value, bool):
        value = "true" if value else "false"
    return f"<{name}>{value}</{name}>\n"


def render_reply(value) -> str:
    """A REPLY to the model, in the same tag notation it writes calls in — one
    notation on the whole model↔harness path, no JSON either way. A dict is one `<k>…</k>` per line; a list
    is its outer tag with one `<item>` per element; text
    with newlines spans lines verbatim; bytes are decoded utf-8 (replace); `None` is
    an empty tag. Why: a JSON envelope escapes every newline and quote, so the model
    cannot see a read's lines, and a refusal carrying a JSON-dumped list shows
    doubled backslashes."""
    if isinstance(value, dict):
        return "".join(_render_reply_field(k, v) for k, v in value.items()).rstrip("\n")
    if isinstance(value, list):
        return _render_reply_field("items", value).rstrip("\n")
    return _reply_text(value)


def _reply_text(v) -> str:
    if v is None:
        return ""
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, bytes):
        return v.decode("utf-8", "replace")
    return str(v)


def _render_reply_field(name: str, value) -> str:
    if isinstance(value, dict):
        inner = "".join(_render_reply_field(k, v) for k, v in value.items())
        return f"<{name}>\n{inner}</{name}>\n"
    if isinstance(value, list):
        # Each element on its own: a dict/list one nests, a scalar one is text — so a
        # MIXED list (a reflect `tree=ids` with pruned stubs) renders both right.
        inner = "".join(
            _render_reply_field("item", v)
            if isinstance(v, (dict, list))
            else f"<item>{_reply_text(v)}</item>\n"
            for v in value
        )
        return f"<{name}>\n{inner}</{name}>\n"
    text = _reply_text(value)
    if "\n" in text:
        return f"<{name}>\n{text}\n</{name}>\n"
    return f"<{name}>{text}</{name}>\n"


def _mint_id() -> str:
    return f"call_{secrets.token_hex(4)}"


def _envelopes(text: str):
    """Yield `(start, body_start, end)` for every `<call …>` envelope in document
    order — `end` is the index of its `</call>` or -1 when unterminated (the last
    thing in the completion). `<call` must be followed by whitespace, so `<caller>`
    is not an envelope; a `</call>` inside a `<bytes>` value does not close."""
    i = 0
    while i < len(text):
        pos = text.find(OPEN, i)
        if pos == -1:
            return
        after = pos + len(OPEN)
        # An envelope is `<call` + whitespace + `name=…`. Anything else (`<caller>`,
        # "a <call soon") is prose: step over the OPENER ONLY, so a mention never
        # swallows the real call that follows it.
        if (
            after >= len(text)
            or not text[after].isspace()
            or not text[after:].lstrip().startswith("name")
        ):
            i = after
            continue
        end = _find_close(text, after)
        yield pos, after, end
        if end == -1:
            return
        i = end + len(CLOSE)


def _scan(text: str) -> list[tuple[str, dict | None, str | None, bool]]:
    """Every envelope found, in document order, as
    `(block, call|None, fault|None, terminated)`.

    A `None` call with a fault means an envelope WAS opened as a call and its body did
    not parse — kept, so `malformed_calls` can say so. A `None` call without a fault
    is the model MENTIONING `<call` in prose (not call-shaped): stepped over, so a
    stray mention never swallows the real call after it."""
    out: list[tuple[str, dict | None, str | None, bool]] = []
    for _pos, after, end in _envelopes(text):
        # UNTERMINATED (`end == -1`) is not a truncation guess: the envelope is the
        # LAST thing in the completion, so there is nothing after it to misread.
        block = text[after : end if end != -1 else len(text)]
        try:
            call = parse_call(block)
            fault = None
        except _Malformed as exc:
            call, fault = None, str(exc)
        if call is None and fault is None:
            continue
        out.append((block, call, fault, end != -1))
    return out


def parses(block: str) -> bool:
    """Whether one envelope body (` name="…">…`, the text after `<call`) is a call
    `extract_tool_calls` would dispatch — the streaming splitter asks it at `</call>`."""
    try:
        return parse_call(block) is not None
    except _Malformed:
        return False


def terminated_calls(text: str) -> list[tuple[dict, str]]:
    """Every dispatchable call with the BODY it was written as (the text after its
    `name="…">`), in document order — what links a call to the stream part it was
    previewed as."""
    out: list[tuple[dict, str]] = []
    for block, call, _fault, done in _scan(text):
        if call is not None and done:
            out.append((call, block[block.index(">") + 1 :]))
    return out


def extract_tool_calls(text: str) -> list[dict]:
    """Non-streaming: pull every finalized `<call>` out of a complete string. A call
    with no `</call>` is NOT a call: the model stopped generating
    before it closed, so nothing is dispatched — the turn's cut marker names it as not
    sent (`stream_parts.cut_marker`)."""
    return [call for _, call, _f, done in _scan(text) if call is not None and done]


# The shape appended to every refusal: literal, minimal, copyable. Advice, not a
# repair — the block is never re-parsed leniently.
_HINT = (
    'The shape: <call name="send">\n<target_id>agent id</target_id>\n'
    "<type>verb</type>\n<path>a.md</path>\n<bytes>document, verbatim, to the first "
    "</bytes>\n</bytes>\n</call> — one child tag per argument, a list is one tag "
    "whose children are all <item>, nothing is escaped."
)


def _defang(text: str) -> str:
    """Neutralize envelope delimiters so quoted text cannot re-arm the scanner.

    Anything we echo back becomes conversation the model reads, quotes and reasons
    about — and a quoted `<call` is indistinguishable from an emitted one: echoing a
    block verbatim makes this error self-amplifying, each report seeding the next. Breaking the opener is enough."""
    for tag in (OPEN, CLOSE, BYTES_OPEN, BYTES_CLOSE):
        text = text.replace(tag, tag.replace("<", "‹").replace(">", "›"))
    return text


def malformed_calls(text: str) -> list[dict]:
    """Envelopes that opened as a call and did not parse, as
    `{error, envelope, your_block}`.

    Exists because a call that cannot be read is the one failure a model cannot
    debug: `extract_tool_calls` returns `[]` for it, which is byte-identical to a
    turn that emitted no call at all — and that means "done": nothing parsed, nothing
    dispatched, no error back, and the run ends looking finished with the task
    untouched. Models self-correct from errors that reach them, so the fix is not a better parser, it
    is giving this failure the same voice the others already have."""
    out: list[dict] = []
    for block, call, fault, terminated in _scan(text):
        # An UNTERMINATED block is not malformed — it was cut; the cut marker says so.
        if call is not None or not terminated:
            continue
        out.append(
            {
                # The whole reply body, composed HERE so the wording has one owner.
                "error": f"call NOT dispatched: {fault}. Nothing ran. " + _HINT,
                "envelope": OPEN,
                # BOTH ENDS, never just the head — the fault in a long block is
                # almost never in the first 400 chars. Shown WHOLE, DEFANGED so
                # quoting it cannot arm the scanner again.
                "your_block": _defang(block.strip()),
            }
        )
    return out
