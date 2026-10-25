"""The CALL CHECKER — what a model wrote as a call that is not one.

A model may invent any call syntax — a `<tool_call>` wrapper, `<function=call
name="send">`, `<send>` as a tag, a JSON object, `<parameter=x>` arguments — and none
of it opens a `<call` envelope, so the scanner finds nothing and the turn reads as
call-less (e.g. `<tool_call><function=call name="send">…</call>`, each answered with
the call-less reminder, the same shape repeated until the chain gives up).

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

from ai_core.grammar import envelopes_in, open_tags
from ai_core.tool_parse import (
    CLOSE,
    TOOLS,
    _ROUTING,
    _Malformed,
    _defang,
    _elements,
    _envelopes,
    parse_call,
)

#: The argument tags a call carries that are ours by name: the routing fields and
#: the two every payload has.
ARG_TAGS = (*_ROUTING, "type", "bytes")
#: At most this many malformed calls are reported per answer, then a count.
MAX_REPORTS = 3
_SHOWN = 160  # chars of a quoted line

_TAG = re.compile(r"<(/?)([^\s<>/=]*)([^<>\n]{0,300}?)(/?)>")
#: A call opener whose leading `<` was dropped — `call name="await">` (typically
#: right after `</think>`; an `<ids>` can lose its `<` the same way). Not a tag to
#: `_TAG`, so without this only its `</call>` is seen and the report says to delete
#: that.
#: Only at the start of the text or after whitespace — where the dropped `<` was
#: (`</think>\n\n<call`); `<function=call name="send">` is another shape.
_BARE = re.compile(r"""(?<!\S)call\s+name\s*=\s*["']?([A-Za-z_]\w*)["']?\s*>""")
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
    bare: bool = False  # a call opener written without its leading `<`


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
    for m in _BARE.finditer(masked):
        toks.append(
            _Tok("opener", m.group(0), m.start(), m.end(), name=m.group(1), bare=True)
        )
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


LEFT_OUT = "LEFT OUT — write the command yourself"
_STRIP_TAGS = re.compile(r"<[^<>]*>")
_GIT_RISKY = [
    ("git stash", re.compile(r"\bgit\b[^;&|\n]*?\sstash\b")),
    (
        "git reset --hard",
        re.compile(r"\bgit\b[^;&|\n]*?\sreset\b[^;&|\n]*\s--hard\b"),
    ),
    (
        "git clean -f",
        re.compile(
            r"\bgit\b[^;&|\n]*?\sclean\b[^;&|\n]*\s(?:-[A-Za-z]*f[A-Za-z]*|--force)\b"
        ),
    ),
    (
        "git push --force",
        re.compile(
            r"\bgit\b[^;&|\n]*?\spush\b[^;&|\n]*\s"
            r"(?:--force(?:-with-lease)?|-[A-Za-z]*f[A-Za-z]*)\b"
        ),
    ),
]
_RM = re.compile(r"\brm((?:\s+-[A-Za-z-]+)+)")


def _rm_rf(line: str) -> bool:
    for m in _RM.finditer(line):
        flags = m.group(1).split()
        short = [f for f in flags if f.startswith("-") and not f.startswith("--")]
        rec = "--recursive" in flags or any(("r" in f or "R" in f) for f in short)
        frc = "--force" in flags or any("f" in f for f in short)
        if rec and frc:
            return True
    return False


def _flat(v: object) -> str:
    if isinstance(v, (list, tuple)):
        return " ".join(str(x) for x in v)
    return str(v)


def _risky(args: list[tuple[str, object]]) -> str | None:
    """The label of a destructive command a start/run call would execute (#261)."""
    if not any(k == "type" and _flat(v).strip() in ("start", "run") for k, v in args):
        return None
    line = " ".join(_flat(v) for k, v in args if k in ("command", "args"))
    line = _STRIP_TAGS.sub(" ", line)
    for label, rx in _GIT_RISKY:
        if rx.search(line):
            return label
    return "rm -rf" if _rm_rf(line) else None


def _rewrite(name: str, args: list[tuple[str, object]]) -> list[str]:
    label = _risky(args)
    if label is None:
        return ["write it as:", _expected(name, args)]
    shown = [(k, LEFT_OUT if k in ("command", "args") else v) for k, v in args]
    return [
        f"careful: this command runs `{label}`, which changes state outside this task "
        "(a stash, reset, clean or force-push acts on the whole repository, and every "
        "worktree shares it; rm -rf deletes for good). It is left out of the form "
        "below: if you still mean it, write the command yourself, and prefer one that "
        "touches only your own files.",
        "write it as:",
        _expected(name, shown),
    ]


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
    if first.bare:
        why = "the opening `<` is missing"
    elif first.kind == "json":
        args = list(_json_args(masked, first).items())
        why = "calls are tags, not JSON"
    elif first.kind in ("arg", "param"):
        why = "an argument outside a call — open the call above it"
    elif first.kind == "close":
        why = "a close with no call open"
    else:
        why = "not a call tag"
    # EVERY ARGUMENT THE MODEL WROTE carries into the form to write — the verb's own
    # (`<query>`, `<ids>`) as much as ours by name: reported with only
    # target/title/type, a grep scoped by `<ids>` is rewritten unscoped. A tag
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
        out += _rewrite(name, args)
    if notes:
        out.append("also: " + "; ".join(notes) + ".")
    out.append(
        'A call is <call name="…">, one child tag per argument, closed by </call>.'
    )
    return "\n".join(out)


def repair(answer: str) -> str:
    """The answer with every call opener that lost its leading `<` given it back —
    `call name="X">…</call>` → `<call name="X">…</call>` — when a `</call>` closes it.
    The spell checker: the `<` is not the model's — vLLM's streaming reasoning
    parser drops the content that shares a delta with `</think>`, so the call is restored and
    runs, and the stored answer shows it whole. Outside every `<call` envelope
    only; anything else stays the checker's to report."""
    masked = _mask(answer)
    at = [
        m.start() for m in _BARE.finditer(masked) if masked.find(CLOSE, m.end()) != -1
    ]
    for i in reversed(at):
        answer = answer[:i] + "<" + answer[i:]
    return answer


def _wrapped_reports(answer: str) -> list[str]:
    """One report per call written INSIDE a wrapper tag: refused, not dispatched.
    The report names the wrapper and gives the bare call to write."""
    out: list[str] = []
    for env in envelopes_in(answer):
        if env.wrapper is None or env.end is None:
            continue
        line = answer.count("\n", 0, env.wrapper.start) + 1
        block = answer[env.body : env.end]
        body = block[block.find(">") + 1 :]
        head = (
            f"[malformed call — nothing ran] line {line}: a call inside "
            f"`<{env.wrapper.name}>` is refused — drop the wrapper; write it at the top level"
        )
        try:
            call = parse_call(block)
            if call is None:
                continue
            args = [(k, raw.strip()) for k, raw in _elements(body)]
        except _Malformed:
            bare = "<" + "call" + block + CLOSE
            out.append(head + f":\n{bare}")
            continue
        out.append("\n".join([head + ".", *_rewrite(call["name"], args)]))
    return out


def _wrapper_only(answer: str) -> list[str]:
    """An answer with no call envelope and an open wrapper that holds no call."""
    if envelopes_in(answer):
        return []
    opens = open_tags(answer)
    if not opens:
        return []
    w = opens[0]
    line = answer.count("\n", 0, w.start) + 1
    example = _expected("send", [("target_id", "agent id"), ("type", "verb")])
    return [
        f"[malformed call — nothing ran] line {line}: `<{w.name}>` holds no call — "
        'write one at the top level, as <call name="…">…</call>.\n'
        "write it as:\n" + example + "\n"
        'If it is finished, end the chain explicitly: <call name="done"></call>'
    ]


def check(answer: str, *, ran: int) -> list[str]:
    """One report per call the answer wrote outside every `<call` envelope, in
    document order (at most `MAX_REPORTS`, then a count); `[]` when there is none.
    `ran` is how many of the answer's calls were dispatched — said in each report,
    so the model knows its well-formed calls are not to be written again."""
    masked = _mask(answer)
    calls = _calls(masked, _tokens(masked))
    found = [_report(answer, masked, c) for c in calls] + _wrapped_reports(answer)
    if not found:
        found = _wrapper_only(answer)
    reports = found[:MAX_REPORTS]
    if len(found) > MAX_REPORTS:
        reports[-1] += f"\n({len(found) - MAX_REPORTS} more malformed calls not shown.)"
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
