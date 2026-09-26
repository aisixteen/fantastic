"""The on-disk form of kernel-written state: YAML, block style, one emitter config.

Everything written under `.fantastic` goes through `dump` here: the records
(`agent.yaml`) and the per-agent sidecars (the scheduler's `schedules.yaml`, and
its `schedule_events.yaml` — a YAML STREAM, one `---` document per event, via
`row_to_doc`). The rust and swift ports reproduce the output BYTE FOR BYTE, so
every choice below is a rule, pinned by the python-written fixture
`tests/fixtures/agent.yaml`.

There is no message tape: the kernel keeps no conversation, and its execution
rows live in its memory store (see the kernel's `flush_executions`).

EMITTER RULES (libyaml — PyYAML `CSafeDumper`; Yams and serde_yml wrap the same
emitter, so the same requested styles give the same bytes):

  * block style (`default_flow_style=False`); an EMPTY map/list is the flow `{}` /
    `[]` libyaml writes for it.
  * indent 2; a list under a key is NOT indented (`key:\\n- a`), libyaml's default.
  * keys in INSERTION order (`sort_keys=False`) — a record or row reads back with
    the order it was written in.
  * `allow_unicode=True`: BMP text is written verbatim; libyaml escapes characters
    outside the BMP inside a double-quoted scalar (`"\\U0001F600"`).
  * no line folding: `width=-1` (unbounded) — a long string is one line.
  * no anchors/aliases, ever (a repeated object is written out again).
  * no `...` document-end marker. libyaml only writes one after a keep-chomped
    (`|+`) block scalar, so that style is never requested (rule below).
  * STRING STYLE is the one choice made here, per string:
      - contains `\\n` and does NOT end in `\\n\\n` (and is not exactly `\\n`) →
        request LITERAL `|` (libyaml picks the chomping indicator `|`/`|-`, and
        itself downgrades to double-quoted when a literal cannot hold the text —
        e.g. a space before a line break, a tab, a `\\r`);
      - contains `\\n` and ends in `\\n\\n` (or is `\\n`) → request DOUBLE-QUOTED;
      - otherwise → no request: plain when the YAML 1.1 resolver reads it back as
        the same string, else single-quoted (`'1'`, `'true'`, `'null'`, `''`,
        `'yes'`, `'on'`, `'2026-09-25'`, `'a: b'`, `'*'`) — PyYAML's
        `represent_str` rule, which the ports mirror;
      - PLUS one rule of ours: a string the YAML 1.2 CORE schema would read as a
        number is single-quoted too — `0o17` (1.2 octal) and a float with no dot
        or an unsigned exponent (`1e3`, `2E-4`, `5.`). The 1.1 resolver reads
        those as strings and would write them plain, but a 1.2 loader (the
        rust/swift ports) would read a number — so they are quoted, and the file
        means the same thing to every runtime. Regex (fullmatch):
        `[-+]?0o[0-7]+|[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?`
        (a string the 1.1 resolver already quotes is quoted either way).
  * scalars: int as decimal, float as PyYAML `repr` form (`1.5`, `.inf`, `.nan`),
    bool `true`/`false`, None `null` (kept, not omitted).

THE ONE REDUCTION: a raw `bytes`/`bytearray` value anywhere (a `write_stream` body,
a `read_stream` chunk) is written as `{len: N}` — base64 is banned and a length is
the honest record; durable bytes live behind `file_bridge`. A tuple is a list. A
value of any other non-YAML type is written as `str(value)`; a non-scalar map key
as `str(key)`. Everything else — payloads, outcomes, bodies, keys like `$parent` —
is verbatim.
"""

from __future__ import annotations

import re
from typing import Any

import yaml

SEPARATOR = "---\n"


# ── write ──────────────────────────────────────────────────────────────────────


class _Dumper(yaml.CSafeDumper):
    def ignore_aliases(self, data: Any) -> bool:
        return True


_CORE_12_NUMBER = re.compile(
    r"[-+]?0o[0-7]+|[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?"
)


def _represent_str(dumper: yaml.CSafeDumper, s: str):
    if "\n" in s:
        style = '"' if (s == "\n" or s.endswith("\n\n")) else "|"
        return dumper.represent_scalar("tag:yaml.org,2002:str", s, style=style)
    if _CORE_12_NUMBER.fullmatch(s):
        return dumper.represent_scalar("tag:yaml.org,2002:str", s, style="'")
    return dumper.represent_str(s)


_Dumper.add_representer(str, _represent_str)

_SCALARS = (str, int, float, bool, type(None))


def plain(v: Any) -> Any:
    """A fresh, plain-typed copy the safe dumper can write (bytes → `{len}`)."""
    if isinstance(v, (bytes, bytearray)):
        return {"len": len(v)}
    if isinstance(v, dict):
        return {
            (k if isinstance(k, _SCALARS) else str(k)): plain(x) for k, x in v.items()
        }
    if isinstance(v, (list, tuple)):
        return [plain(x) for x in v]
    if isinstance(v, bool) or v is None:
        return v
    if isinstance(v, str):
        return str.__str__(v)
    if isinstance(v, int):
        return int(v)
    if isinstance(v, float):
        return float(v)
    return str(v)


def dump(obj: Any) -> str:
    """One YAML document (no `---`, no `...`) by the rules above."""
    return yaml.dump(
        plain(obj),
        Dumper=_Dumper,
        default_flow_style=False,
        sort_keys=False,
        allow_unicode=True,
        indent=2,
        width=-1,
    )


def load(text: str) -> Any:
    """One YAML document back (CSafeLoader). Raises `yaml.YAMLError` on bad input."""
    return yaml.load(text, Loader=yaml.CSafeLoader)


def row_to_doc(row: dict) -> str:
    """One entry of a YAML stream as one document: `---\\n` + the block mapping."""
    return SEPARATOR + dump(row)
