"""The call grammar: envelopes, wrappers, and the code that is not markup.

The grammar half of Rust #189, ported to python. `code_masked` blanks code so a
backtick in prose cannot open a span that hides a wrapper; `track` walks the tags
of a text; `envelopes_in` pairs every envelope with the tag it was written
inside. Nothing imports this yet — wiring it into the checker and the stream
splitter is part 2.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

# Delimiters are built by concatenation: this file must never type the opener or
# the document closer literally, or the checker reads it as a call.
OPEN = "<" + "call"
CLOSE = "</" + "call" + ">"
BYTES_OPEN = "<" + "bytes" + ">"
BYTES_CLOSE = "</" + "bytes" + ">"
THINK_OPEN = "<" + "think" + ">"
THINK_CLOSE = "</" + "think" + ">"

# A tag: a name, then nothing, or attributes written name="…", name='…' or
# name=value. Prose such as `a<b and c>d` has bare words where an attribute
# would be, so it is not a tag.
TAG = re.compile(
    r"""<(/?)([A-Za-z_][\w.\-]*)"""
    r"""((?:\s+[\w:.\-]+\s*=\s*(?:"[^"<>\n]*"|'[^'<>\n]*'|[^\s"'<>=`]+))*\s*)(/?)>"""
)


def find_close(text: str, start: int) -> int | None:
    """Index of the closer of a call whose body starts at `start`, skipping any
    closer inside a `<bytes>` value; None when the envelope is unterminated."""
    i = start
    while True:
        c = text.find(CLOSE, i)
        b = text.find(BYTES_OPEN, i)
        if b == -1:
            return c if c != -1 else None
        if c != -1 and c < b:
            return c
        e = text.find(BYTES_CLOSE, b + len(BYTES_OPEN))
        if e == -1:
            return None  # an open document runs to the end: unterminated
        i = e + len(BYTES_CLOSE)


def envelopes(text: str) -> list[tuple[int, int, int | None]]:
    """`(start, body_start, end)` for every envelope in document order. The opener
    must be followed by whitespace and a `name`, so a mention in prose is stepped
    over — never swallowing the real call that follows it. `end` is None when the
    envelope is unterminated: the last thing in the completion, so there is nothing
    after it to misread."""
    out: list[tuple[int, int, int | None]] = []
    i = 0
    while i < len(text):
        pos = text.find(OPEN, i)
        if pos == -1:
            return out
        after = pos + len(OPEN)
        rest = text[after:]
        s = rest.lstrip()
        if not (rest and rest[0].isspace()) or not (s.startswith("name") and s[4:].lstrip(" ").startswith("=")):
            i = after
            continue
        end = find_close(text, after)
        out.append((pos, after, end))
        if end is None:
            return out
        i = end + len(CLOSE)
    return out


@dataclass
class OpenTag:
    """A tag the model opened and has not closed yet: its span and its name."""

    start: int
    end: int
    name: str


@dataclass
class Envelope:
    """One envelope, with the tag it was written inside, if any.

    `wrapper` is the OUTERMOST tag still open when the envelope starts — a call
    inside a `tool_call` tag is markup, not a top-level call. None for a call at
    the top level."""

    start: int
    body: int
    end: int | None
    wrapper: OpenTag | None


def _blank(s: str) -> str:
    """Blank a string, keeping its newlines and its length."""
    return "".join("\n" if c == "\n" else " " for c in s)


def blank_thinking(text: str) -> str:
    """Blank the inside of every think region and keep its tags, so a backtick in
    thinking cannot open a code span that runs across the closer and hides it. The
    region stays an open tag, so a call inside it is still wrapped. An unclosed
    region runs to the end of the text."""
    out: list[str] = []
    at = 0
    while True:
        off = text.find(THINK_OPEN, at)
        if off == -1:
            break
        body = off + len(THINK_OPEN)
        out.append(text[at:body])
        stop = text.find(THINK_CLOSE, body)
        if stop == -1:
            stop = len(text)
        out.append(_blank(text[body:stop]))
        at = stop
        if at == len(text):
            break
        out.append(THINK_CLOSE)
        at += len(THINK_CLOSE)
    out.append(text[at:])
    return "".join(out)


def _lines_with_ends(text: str) -> list[str]:
    """Split on newlines, each piece keeping its own; the last piece has none."""
    parts = text.split("\n")
    out = [p + "\n" for p in parts[:-1]]
    if parts[-1]:
        out.append(parts[-1])
    return out


def blank_fences(text: str) -> str:
    """Blank every fenced block: a line of three or more backticks or tildes opens
    it, and a line of the same mark, at least as long, closes it. Unclosed, it runs
    to the end of the text."""
    out: list[str] = []
    fence: tuple[str, int] | None = None
    for line in _lines_with_ends(text):
        t = line.lstrip(" ")
        mark = t[0] if t and t[0] in "`~" else None
        run = 0
        if mark:
            run = len(t) - len(t.lstrip(mark))
        if fence is None:
            if run >= 3:
                fence = (mark, run)
                out.append(_blank(line))
            else:
                out.append(line)
        else:
            out.append(_blank(line))
            m, n = fence
            if mark == m and run >= n and not t[run:].strip():
                fence = None
    return "".join(out)


def _backticks_at(text: str, at: int) -> int:
    """The number of backticks in the run at `at`."""
    n = 0
    while at + n < len(text) and text[at + n] == "`":
        n += 1
    return n


def _closer(text: str, from_: int, length: int) -> int | None:
    """The index just past the closing run of `length` backticks, searching from
    `from_`; None when a blank line comes first or the text ends."""
    i = from_
    while True:
        off = text.find("`", i)
        if off == -1:
            return None
        p = off
        if "\n\n" in text[i:p]:
            return None
        run = _backticks_at(text, p)
        if run == length:
            return p + run
        i = p + run


def blank_spans(text: str) -> str:
    """Blank every code span: a run of backticks, through the next run of the SAME
    length, with no blank line between."""
    out: list[str] = []
    last = 0
    i = 0
    while True:
        off = text.find("`", i)
        if off == -1:
            break
        start = off
        run = _backticks_at(text, start)
        end = _closer(text, start + run, run)
        if end is None:
            i = start + run
        else:
            out.append(text[last:start])
            out.append(_blank(text[start:end]))
            last = end
            i = end
    out.append(text[last:])
    return "".join(out)


def code_masked(text: str) -> str:
    """The text with every code fence and code span blanked, for the tag walk only:
    code is not markup, so it opens no wrapper. Every character keeps its offset
    (each becomes a space, a newline stays), so positions in the masked text are
    positions in the answer. A span needs its closing backticks of the same run
    length before a blank line; an unmatched backtick is a plain character."""
    return blank_spans(blank_fences(blank_thinking(text)))


def track(text: str, start: int, end: int, open_tags: list[OpenTag]) -> None:
    """Walk the tags of `text[start:end]`: an open tag is pushed; a close pops the
    most recent open tag of its name, and whatever opened after it. A close with no
    open tag of its name is stray and does nothing. A self-closing tag is not
    opened."""
    for m in TAG.finditer(text[start:end]):
        name = m.group(2) or ""
        if m.group(1):
            for i in range(len(open_tags) - 1, -1, -1):
                if open_tags[i].name == name:
                    del open_tags[i:]
                    break
        elif not m.group(4):
            open_tags.append(OpenTag(start + m.start(), start + m.end(), name))


def _blank_envelopes(text: str, spans: list[tuple[int, int, int | None]]) -> str:
    """`text` with every envelope blanked, its newlines kept. Code is masked over
    the prose only, as the stream splitter reads it: a backtick inside a call's
    argument must not open a code span that runs on past the call and hides a
    wrapper after it."""
    out: list[str] = []
    at = 0
    for start, _, end in spans:
        stop = len(text) if end is None else end + len(CLOSE)
        out.append(text[at:start])
        out.append(_blank(text[start:stop]))
        at = stop
    out.append(text[at:])
    return "".join(out)


def envelopes_in(text: str) -> list[Envelope]:
    """Every envelope in document order, each with the tag it sits inside. A tag
    closed before the call wraps nothing."""
    spans = envelopes(text)
    masked = code_masked(_blank_envelopes(text, spans))
    out: list[Envelope] = []
    open_tags: list[OpenTag] = []
    at = 0
    for start, body, end in spans:
        track(masked, at, start, open_tags)
        out.append(Envelope(start, body, end, open_tags[0] if open_tags else None))
        at = len(text) if end is None else end + len(CLOSE)
    return out


def open_tags(text: str) -> list[OpenTag]:
    """The tags still open at the end of `text`, outermost first, code excluded.
    Meant for a text with no envelope in it: an envelope's own tags are not walked
    by `envelopes_in`, and this does not skip them."""
    masked = code_masked(text)
    out: list[OpenTag] = []
    track(masked, 0, len(masked), out)
    return out
