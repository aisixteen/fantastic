"""AUTOFOLD (#556): the model's view of a row as an anchor, at assembly time.

One pure, deterministic pass: nothing is stored. A thought, a cut answer and an answered
large or recalled reply show as a short anchor naming the row's `id`; `recall` with the
id reads the row back whole. Twin of the Rust `history_view::autofold`, byte for byte.

It imports no `core`: `core` and `caching` import it, so it carries its own copies of the
reply-tag patterns.
"""

from __future__ import annotations

import re

from ai_core.constants import AUTOFOLD_AT, CHARS_PER_TOKEN
from ai_core.tool_parse import terminated_calls

_REPLY_TAG = re.compile(r"^\[reply to \S+ · (\S+) ([^\]\s]+)[^\]]*\]")
_BLOCK_SPLIT = re.compile(r"\n\n(?=\[reply to \S+ · )")
_TAG_LINKS = re.compile(r"^\[reply to \S+ · [^\]]*?↑([^\s\]]+)[^\]]*\]")


def _as_id(x) -> int | None:
    """A row id as an int — an int, or its decimal string."""
    if isinstance(x, bool):
        return None
    if isinstance(x, int):
        return x if x > 0 else None
    if isinstance(x, str) and x.strip().isascii() and x.strip().isdigit():
        return int(x.strip())
    return None


def _tokens_of(n: int) -> int:
    return (n + CHARS_PER_TOKEN - 1) // CHARS_PER_TOKEN


def _tokens(text: str) -> int:
    return _tokens_of(len(text))


def _body(block: str) -> str:
    """The text of a block after its first line (the tag line)."""
    return block.split("\n", 1)[1] if "\n" in block else ""


def _tag(block: str) -> tuple[str, str] | None:
    m = _REPLY_TAG.match(block)
    return (m.group(1), m.group(2)) if m else None


def _correction(content: str) -> str | None:
    """The body of the correction `content` carries: a block tagged `- -` whose body
    opens with the malformed-call or unsupported-tag report."""
    for b in _BLOCK_SPLIT.split(content):
        if _tag(b) != ("-", "-"):
            continue
        body = _body(b)
        if body.startswith("[malformed call — nothing ran]") or body.startswith(
            "[unsupported tag — not read]"
        ):
            return body
    return None


def _thinking_tokens(row: dict) -> int | None:
    t = row.get("thinking")
    if not isinstance(t, str) or not t:
        return None
    cut = row.get("thinking_cut")
    cut = cut if isinstance(cut, int) and not isinstance(cut, bool) and cut > 0 else 0
    return _tokens_of(len(t) + cut)


def _cut_content(content: str, thought: int | None, rid: int, correction: str) -> str:
    c = content.rstrip()
    if "\n" in c:
        i = c.rindex("\n")
        prefix, last = c[:i].rstrip(), c[i + 1 :].strip()
    else:
        prefix, last = "", c.strip()
    wrote = last[:120]
    said = "" if thought is None else f" after thinking ~{thought} tokens"
    lines = correction.split("\n")
    if "write it as:" in lines:
        form = ""
        for line in lines[lines.index("write it as:") + 1 :]:
            form += line
            if line.endswith("</call>"):
                break
        action = f"rewrite as {form}"
    else:
        caret = lines[2].strip() if len(lines) > 2 else ""
        caret = caret.lstrip("^~")
        action = caret[1:] if caret.startswith(" ") else caret
    anchor = (
        f"[cut · you wrote {wrote}{said} · nothing ran · ACTION: {action} · full: {rid}]"
    )
    return anchor if not prefix else f"{prefix}\n{anchor}"


def _recalled_what(hist: list[dict], tag_line: str, agent: str, nth: int) -> str:
    """What a recall reply recalled (` · id N` or ` · query "q"`), read off the model's
    own recall call the block answers; empty when any step fails."""
    m = _TAG_LINKS.match(tag_line)
    n = _as_id(m.group(1)) if m else None
    if n is None:
        return ""
    row = next(
        (r for r in hist if r.get("id") == n and r.get("role") == "assistant"), None
    )
    if row is None:
        return ""
    calls = [
        c
        for c, _ in terminated_calls(str(row.get("content") or ""))
        if c.get("name") == "send"
        and (c.get("arguments") or {}).get("target_id") == agent
        and isinstance((c["arguments"].get("payload")), dict)
        and c["arguments"]["payload"].get("type") == "recall"
    ]
    if nth >= len(calls):
        return ""
    payload = calls[nth]["arguments"]["payload"]
    rid = _as_id(payload.get("id"))
    if rid is not None:
        return f" · id {rid}"
    q = payload.get("query")
    if isinstance(q, str) and q:
        return f' · query "{q}"'
    return ""


def will_fold(content: str) -> bool:
    """True when a user turn will fold once answered: it opens with a reply tag and
    carries a recall reply or a body over AUTOFOLD_AT tokens."""
    if not _REPLY_TAG.match(content):
        return False
    for b in _BLOCK_SPLIT.split(content):
        t = _tag(b)
        if (t is not None and t[1] == "recall") or _tokens(_body(b)) > AUTOFOLD_AT:
            return True
    return False


def autofold_row(hist: list[dict], i: int, msg: dict, incoming: str | None) -> dict:
    """The row `hist[i]`, whose current message is `msg`, as the model sees it. Pure
    and never stored. `incoming` is the text of the turn being assembled: the
    correction a last cut row is followed by. A row with no int `id` comes back
    unchanged."""
    row = hist[i]
    content = msg.get("content")
    rid = row.get("id")
    if not isinstance(content, str) or not isinstance(rid, int) or isinstance(rid, bool):
        return msg
    role = row.get("role")
    if role == "assistant":
        nxt = hist[i + 1].get("content") if i + 1 < len(hist) else incoming
        thought = _thinking_tokens(row)
        corr = _correction(nxt) if isinstance(nxt, str) else None
        if corr is not None:
            return {**msg, "content": _cut_content(content, thought, rid, corr)}
        if thought is None:
            return msg
        anchor = f"[thought ~{thought} tokens · id {rid}]"
        return {**msg, "content": f"{anchor}\n{content}" if content else anchor}
    if role == "user":
        answered = any(t.get("role") == "assistant" for t in hist[i + 1 :])
        if not answered or not _REPLY_TAG.match(content):
            return msg
        shown: list[str] = []
        changed = False
        recalls = 0
        for b in _BLOCK_SPLIT.split(content):
            agent, verb = _tag(b) or ("", "")
            body = _body(b)
            x = _tokens(body)
            if verb == "recall":
                what = _recalled_what(hist, b.split("\n", 1)[0], agent, recalls)
                recalls += 1
                changed = True
                shown.append(
                    f"[folded · recall reply from {agent}{what} · ~{x} tokens · get: {rid}]"
                )
            elif x > AUTOFOLD_AT:
                changed = True
                head = body.split("\n", 1)[0].strip()[:120]
                shown.append(
                    f'[folded · {verb} reply from {agent} · ~{x} tokens · head: "{head}" · get: {rid}]'
                )
            else:
                shown.append(b)
        return {**msg, "content": "\n\n".join(shown)} if changed else msg
    return msg
