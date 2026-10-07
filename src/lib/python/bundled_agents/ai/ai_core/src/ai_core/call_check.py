"""The CALL CHECKER — what a model wrote as a call that is not one (owner, 2026-10-07).

A model may invent any call syntax — a `<tool_call>` wrapper, `<function=call
name="send">`, `<send>` as a tag, a JSON object, `<parameter=x>` arguments — and none
of it opens a `<call` envelope, so the scanner finds nothing and the turn reads as
call-less (measured 2026-10-07, E59 `calc` on qwen3.8-27b: 18 of 25 replies were
`<tool_call><function=call name="send">…</call>`, each answered with the call-less
reminder, the same shape repeated until the chain gave up).

The foreign syntaxes cannot be listed, so this checks the other way round: OUR
grammar and OUR vocabulary are closed — `<call name="N">`, one child tag per
argument, `</call>`; the call names (`TOOLS`) and the routing tags (`target_id`, …)
are known — and whatever markup carries that vocabulary OUTSIDE every `<call`
envelope is a call written wrong. Each such call is reported like a compiler error:
the line, a caret under the first token that departs from the grammar, and the form
to write instead, built from the name and arguments the model used (a correction
that names the right form repairs; one that only says "wrong" does not —
arXiv 2607.14167, +42–44 points on Qwen/Llama).

Nothing here runs a call: one grammar, one parser. The answer's valid calls run as
written; the report says how many did.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field

from ai_core.tool_parse import CLOSE, TOOLS, _ROUTING, _defang, _envelopes

#: The argument tags a call carries that are ours by name: the routing fields and
#: the two every payload has.
ARG_TAGS = (*_ROUTING, "type", "bytes")
#: At most this many malformed calls are reported per answer, then a count.
MAX_REPORTS = 3
_SHOWN = 160  # chars of a quoted line

_TAG = re.compile(r"<(/?)([^\s<>/=]*)([^<>\n]{0,300}?)(/?)>")
_NAMED = re.compile(r"""\bname\s*=\s*["']?([A-Za-z_]\w*)""")
_EQ = re.compile(r"""^=\s*["']?([A-Za-z_]\w*)""")
_JSON = re.compile(
    r"""\{\s*"(?:name|tool|tool_name|function)"\s*:\s*"(%s)\"""" % "|".join(TOOLS)
)


@dataclass
class _Tok:
    kind: str  # opener | close | arg | param | other | json
    text: str
    start: int
    end: int
    name: str | None = None  # the call name it carries, when it carries one
    arg: str | None = None  # the argument it opens, for arg / param
    closing: bool = False


@dataclass
class _Call:
    toks: list = field(default_factory=list)
    wrappers: list = field(default_factory=list)


def _mask(text: str) -> str:
    """The answer with every `<call` envelope blanked — those are the scanner's
    (dispatched, or reported as unparsable); newlines kept so lines still count."""
    out = list(text)
    for pos, _after, end in _envelopes(text):
        stop = len(text) if end == -1 else end + len(CLOSE)
        for i in range(pos, stop):
            if out[i] != "\n":
                out[i] = " "
    return "".join(out)


def _classify(m: re.Match) -> _Tok:
    slash, head, rest, _self = m.group(1), m.group(2), m.group(3), m.group(4)
    t = _Tok("other", m.group(0), m.start(), m.end(), closing=bool(slash))
    named = _NAMED.search(rest)
    eq = _EQ.match(rest)
    if head == "call":
        t.kind = "close" if slash else "opener"
        t.name = named.group(1) if named else None
    elif head in TOOLS:
        t.kind, t.name = ("close" if slash else "opener"), head
    elif not slash and named and named.group(1) in TOOLS:
        t.kind, t.name = "opener", named.group(1)
    elif not slash and eq and eq.group(1) in TOOLS:
        t.kind, t.name = "opener", eq.group(1)
    elif not slash and eq and eq.group(1) in ARG_TAGS:
        t.kind, t.arg = "param", eq.group(1)
    elif head in ARG_TAGS:
        t.kind, t.arg = "arg", head
    return t


def _tokens(masked: str) -> list[_Tok]:
    toks = [_classify(m) for m in _TAG.finditer(masked)]
    for m in _JSON.finditer(masked):
        toks.append(_Tok("json", m.group(0), m.start(), m.end(), name=m.group(1)))
    return sorted(toks, key=lambda t: t.start)


def _calls(masked: str, toks: list[_Tok]) -> list[_Call]:
    """The call-like runs: each starts at a call-like token and ends at a closing
    `</call>` / `</<call name>>` (a JSON object is one run). Foreign tags right
    before a run's first token — on its line or the one above — are its wrappers."""
    out: list[_Call] = []
    cur: _Call | None = None
    pending: list[_Tok] = []
    for t in toks:
        if t.kind == "other":
            (cur.toks if cur else pending).append(t)
            continue
        if cur is None:
            cur = _Call()
            first_line = masked.count("\n", 0, t.start)
            cur.wrappers = [
                w
                for w in pending
                if not w.closing and first_line - masked.count("\n", 0, w.start) <= 1
            ]
            pending = []
        cur.toks.append(t)
        if t.kind in ("json", "close"):
            out.append(cur)
            cur = None
    if cur is not None:
        out.append(cur)
    return [c for c in out if any(t.kind != "other" for t in c.toks)]


def _value(masked: str, tok: _Tok, closer: str) -> str | None:
    """The text between an argument's open tag and its close, on the masked answer."""
    end = masked.find(closer, tok.end)
    if end == -1:
        return None
    v = masked[tok.end : end].strip()
    return v if len(v) <= 200 else v[:200] + "…"


def _json_args(masked: str, tok: _Tok) -> dict:
    """The arguments of a JSON call object, flattened one level (a `payload` /
    `arguments` / `parameters` object's keys become tags of the call)."""
    try:
        obj, _ = json.JSONDecoder().raw_decode(masked[tok.start :])
    except ValueError:
        return {}
    args: dict = {}
    for k, v in obj.items():
        if k in ("name", "tool", "tool_name", "function"):
            continue
        if isinstance(v, dict):
            for k2, v2 in v.items():
                if isinstance(v2, dict):
                    args.update(v2)
                else:
                    args[k2] = v2
        else:
            args[k] = v
    return args


def _expected(name: str, args: list[tuple[str, str]]) -> str:
    if not args:
        return f'<call name="{name}"></call>'
    lines = [f'<call name="{name}">']
    for k, v in args:
        if isinstance(v, list):
            items = "".join(f"<item>{x}</item>" for x in v)
            lines.append(f"<{k}>{items}</{k}>")
        else:
            lines.append(f"<{k}>{v}</{k}>")
    lines.append("</call>")
    return "\n".join(lines)


def _where(text: str, tok: _Tok) -> tuple[int, int, str, str]:
    """(line, col, the quoted line, its caret) — 1-based, the line windowed to
    `_SHOWN` chars around the token, defanged so the quote cannot arm the scanner
    (`_defang` swaps one char for one, so the caret still lines up)."""
    line_start = text.rfind("\n", 0, tok.start) + 1
    line_end = text.find("\n", tok.start)
    line = text[line_start : len(text) if line_end == -1 else line_end]
    col = tok.start - line_start
    lo = max(0, col - 40)
    shown = line[lo : lo + _SHOWN]
    lead = "…" if lo else ""
    width = max(1, min(len(tok.text), len(shown) - (col - lo)))
    caret = " " * (len(lead) + col - lo) + "^" + "~" * (width - 1)
    return text.count("\n", 0, tok.start) + 1, col + 1, lead + _defang(shown), caret


def _plain(t: _Tok) -> str | None:
    """The name of a bare opening tag (`<query>`, no attributes) that is none of
    ours — inside a call run, an argument of the verb."""
    m = re.fullmatch(r"<([A-Za-z_][\w-]*)>", t.text)
    return m.group(1) if t.kind == "other" and m and m.group(1) != "item" else None


def _report(text: str, masked: str, call: _Call) -> str:
    first = next(t for t in call.toks if t.kind != "other")
    name = next((t.name for t in call.toks if t.name), None) or "send"
    args: list[tuple[str, str]] = []
    notes: list[str] = []
    if first.kind == "json":
        args = list(_json_args(masked, first).items())
        why = "calls are tags, not JSON"
    elif first.kind in ("arg", "param"):
        why = "an argument outside a call — open the call above it"
    elif first.kind == "close":
        why = "a close with no call open"
    else:
        why = "not a call tag"
    # EVERY ARGUMENT THE MODEL WROTE carries into the form to write — the verb's own
    # (`<query>`, `<ids>`) as much as ours by name. Measured 2026-10-07
    # (misleading_fold:rust): a grep scoped by `<ids>` lost its `<` and was
    # reported with only target/title/type; the model rewrote it unscoped. A tag
    # nested in an argument (an `<item>` of `<ids>`) is part of that value.
    after = -1
    for t in call.toks:
        if t.start < after:
            continue
        head = _plain(t)
        if (t.kind == "arg" and not t.closing) or head:
            name_ = t.arg if t.kind == "arg" else head
            v = _value(masked, t, f"</{name_}>")
            if v is not None:
                args.append((name_, v))
                after = masked.find(f"</{name_}>", t.end) + len(name_) + 3
        elif t.kind == "param":
            close = re.compile(r"</[^<>\n]{0,40}>").search(masked, t.end)
            v = masked[t.end : close.start()].strip() if close else None
            if v is not None:
                args.append((t.arg, v[:200]))
                notes.append(
                    f"`{_defang(t.text)}` — one child tag per argument: "
                    f"<{t.arg}>…</{t.arg}>"
                )
        elif t.kind == "close" and t.text != CLOSE:
            notes.append(f"`{_defang(t.text)}` — close with </call>")
    notes = [
        f"line {text.count(chr(10), 0, w.start) + 1} `{_defang(w.text)}` — drop the wrapper"
        for w in call.wrappers
    ] + notes
    line, col, quoted, caret = _where(text, first)
    out = [
        f"[malformed call — nothing ran] line {line}, col {col}:",
        f"  {quoted}",
        f"  {caret} {why}",
    ]
    if first.kind == "close" and len([t for t in call.toks if t.kind != "other"]) == 1:
        out[-1] += " — delete it"
    else:
        out += ["write it as:", _expected(name, args)]
    if notes:
        out.append("also: " + "; ".join(notes) + ".")
    out.append(
        'A call is <call name="…">, one child tag per argument, closed by </call>.'
    )
    return "\n".join(out)


def check(answer: str, *, ran: int) -> list[str]:
    """One report per call the answer wrote outside every `<call` envelope, in
    document order (at most `MAX_REPORTS`, then a count); `[]` when there is none.
    `ran` is how many of the answer's calls were dispatched — said in each report,
    so the model knows its well-formed calls are not to be written again."""
    masked = _mask(answer)
    calls = _calls(masked, _tokens(masked))
    reports = [_report(answer, masked, c) for c in calls[:MAX_REPORTS]]
    if len(calls) > MAX_REPORTS:
        reports[-1] += f"\n({len(calls) - MAX_REPORTS} more malformed calls not shown.)"
    if ran:
        tail = (
            "The other call of this answer ran — its reply is in this batch; do not "
            "write it again."
            if ran == 1
            else f"The other {ran} calls of this answer ran — their replies are in "
            "this batch; do not write them again."
        )
        reports = [f"{r}\n{tail}" for r in reports]
    return reports
