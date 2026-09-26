"""The on-disk YAML format — pinned byte-for-byte by a python-written fixture.

`fixtures/agent.yaml` (a record) is what THIS module writes for the input below. The
rust and swift ports' twins load the same file and must write it back byte-identical
(load → dump is the identity on a python-written file), and build the same input in
code and match the bytes. There is no message tape any more: the kernel keeps no
conversation, and its execution rows live in its memory store.
Regenerate after a deliberate format change: `FANTASTIC_REGEN_FIXTURES=1 pytest`.
"""

from __future__ import annotations

import os
from pathlib import Path

from kernel_state import yaml_io
from kernel_state.tools import read_tree, write_record

FIX = Path(__file__).parent / "fixtures"

RECORD = {
    "id": "tricky_0a1b2c",
    "handler_module": "text_file.tools",
    "parent_id": "kernel_state",
    "one": "1",
    "truthy": "true",
    "nully": "null",
    "tilde": "~",
    "empty": "",
    "yes": "yes",
    "on": "on",
    "octal12": "0o17",
    "exp": "1e3",
    "exp_neg": "2E-4",
    "dot_end": "5.",
    "version": "1.2.3",
    "date": "2026-09-25",
    "colon": "a: b",
    "hash": "# not a comment",
    "lead": " leading space",
    "star": "*",
    "unicode": "héllo — 世界",
    "emoji": "ok 😀",
    "int": 42,
    "neg": -7,
    "float": 1.5,
    "bool": True,
    "none": None,
    "multiline": "line one\nline two\n",
    "no_final_nl": "a\n  indented b",
    "trailing_blank": "keep\n\n",
    "space_before_break": "trail \nx",
    "tab": "a\tb",
    "long": "word " * 30,
    "can_call": {"$self": "*", "$parent": "*", "kernel_store": ["read", "write"]},
    "callable_by": {"$parent": "*", "$children": "*"},
    "nested": {"list": [1, "two", {"three": []}], "map": {}, "deep": {"x": [[]]}},
}


def _check(name: str, got: str) -> None:
    path = FIX / name
    if os.environ.get("FANTASTIC_REGEN_FIXTURES"):
        path.write_text(got, encoding="utf-8")
    assert got == path.read_text(encoding="utf-8"), f"{name} drifted"


def test_record_matches_the_fixture_bytes():
    _check("agent.yaml", yaml_io.dump(RECORD))


def test_load_then_dump_is_the_identity_on_the_fixture():
    text = (FIX / "agent.yaml").read_text(encoding="utf-8")
    assert yaml_io.dump(yaml_io.load(text)) == text


def test_record_round_trips_type_stable_and_byte_stable(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    d = Path(".fantastic")
    d.mkdir()
    write_record(d, RECORD)
    first = (d / "agent.yaml").read_text(encoding="utf-8")
    back = read_tree(d)[0]
    assert back == RECORD
    assert [type(v) for v in back.values()] == [type(v) for v in RECORD.values()]
    assert list(back) == list(RECORD)  # key order kept, no sort
    write_record(d, back)
    assert (d / "agent.yaml").read_text(encoding="utf-8") == first
    assert "..." not in first.splitlines()  # never a document-end marker
