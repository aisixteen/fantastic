"""The message log's on-disk form: ONE XML document, rows appended, root never closed.

    <messages v="1">
    <row id="412" agent="ai" kind="body" parent="408" ts="1789000000.12">
    <content>PRJ-1BCA4F

    # Report — a line with &lt;/content&gt; inside is fine, the library escaped it
    </content>
    </row>
    …

Why XML and not JSON lines (user, 2026-09-09): the model↔harness path carries no
JSON, and the XML parsers in the stdlib are the mature streaming ones — the file is
a STREAM whose root is left open, hydrated by `XMLPullParser` one completed `<row>`
at a time; EOF without `</messages>` is the normal end. Written and read by the
library, never by hand: `<` and `&` in a stored body are escaped on the way out and
restored on the way in, so a document that contains `</content>` cannot break the
log. Real newlines stay real.

Mapping (generic, no per-kind schema): a scalar field is an ATTRIBUTE, typed on the
way back by the ONE scalar rule the CLI and the call grammar share
(`kernel.modes.parse.coerce_scalar`: true/false, int, float, else str); a text field
(`content`, `title`, `why`, or any string holding a newline) is a child ELEMENT; a
list is a child element of `<item>`s; a nested object is a child element carrying
its own attributes/children. `None` is omitted (absent on read = None for every
reader that `.get`s). A truncated last row (a crash mid-append) is a PARSE ERROR
naming the byte offset — fail loud, no repair; the operator truncates at the last
good row.
"""

from __future__ import annotations

import xml.etree.ElementTree as ET
from typing import Any

from kernel.modes.parse import coerce_scalar

VERSION = "1"
HEADER = f'<messages v="{VERSION}">\n'
_TEXT_KEYS = frozenset({"content", "title", "why", "text", "summary"})


# ── write ──────────────────────────────────────────────────────────────────────


def _fill(elem: ET.Element, obj: dict) -> None:
    for k, v in obj.items():
        if v is None:
            continue
        if isinstance(v, bool):
            elem.set(k, "true" if v else "false")
        elif isinstance(v, (int, float)):
            elem.set(k, repr(v) if isinstance(v, float) else str(v))
        elif isinstance(v, str):
            if k in _TEXT_KEYS or "\n" in v or "\r" in v:
                child = ET.SubElement(elem, k)
                child.text = v
            else:
                elem.set(k, v)
        elif isinstance(v, (list, tuple)):
            child = ET.SubElement(elem, k)
            for item in v:
                it = ET.SubElement(child, "item")
                _fill_value(it, item)
        elif isinstance(v, dict):
            child = ET.SubElement(elem, k)
            _fill(child, v)
        else:
            elem.set(k, str(v))


def _fill_value(elem: ET.Element, v: Any) -> None:
    if isinstance(v, dict):
        _fill(elem, v)
    elif isinstance(v, (list, tuple)):
        for item in v:
            it = ET.SubElement(elem, "item")
            _fill_value(it, item)
    elif isinstance(v, bool):
        elem.text = "true" if v else "false"
    elif v is None:
        elem.set("null", "true")
    else:
        elem.text = str(v)


def row_to_xml(row: dict) -> str:
    """One row as one `<row>…</row>` element plus a newline."""
    elem = ET.Element("row")
    _fill(elem, row)
    return ET.tostring(elem, encoding="unicode") + "\n"


# ── read ───────────────────────────────────────────────────────────────────────


def _read(elem: ET.Element) -> dict:
    out: dict = {k: coerce_scalar(v) for k, v in elem.attrib.items()}
    for child in elem:
        out[child.tag] = _read_value(child)
    return out


def _read_value(child: ET.Element):
    if child.get("null") == "true" and len(child) == 0 and not child.text:
        return None
    items = [c for c in child if c.tag == "item"]
    if items and len(items) == len(child) and not child.attrib:
        return [_read_value(i) for i in items]
    if len(child) or child.attrib:
        return _read(child)
    text = child.text or ""
    if child.tag in _TEXT_KEYS or "\n" in text:
        return text
    return coerce_scalar(text)


def iter_rows(chunks) -> "list[dict]":
    """Every completed `<row>` in a stream of text chunks, in order. The document's
    root is open-ended: EOF without `</messages>` is normal. A malformed row raises
    `xml.etree.ElementTree.ParseError` with the position — loud, unrepaired."""
    parser = ET.XMLPullParser(events=("end",))
    rows: list[dict] = []
    opened = 0
    for chunk in chunks:
        # A `<row` in a stored body is escaped by the writer, so the raw count is
        # exactly the number of rows the file holds — complete or cut.
        opened += chunk.count("<row ") + chunk.count("<row>")
        parser.feed(chunk)
        for _event, elem in parser.read_events():
            if elem.tag == "row":
                rows.append(_read(elem))
                elem.clear()
    # NO `parser.close()`: closing would demand `</messages>`, and the log is a
    # stream that never writes it. What has been fed and completed is the log —
    # but a row that OPENED and never completed is a cut file, said loudly.
    if opened != len(rows):
        raise ET.ParseError(
            f"messages log: {opened - len(rows)} row(s) opened and never closed — "
            "the last append was cut (a crash mid-write). Truncate the file at the "
            f"last complete </row> (row {rows[-1]['id'] if rows else 'none'}) — "
            "nothing is repaired automatically"
        )
    return rows


def parse_messages(text: str) -> list[dict]:
    return iter_rows([text])
