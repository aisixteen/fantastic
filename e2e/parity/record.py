"""Every byte a family writes goes through here.

ONE WRITER. The round-1/2 scripts each had their own `save`: three of them
passed `sort_keys=True` (`parity_capture.py:76`, `parity_web.py:10`,
`parity_capture_cli_ctx.py:95`), which throws away key ORDER — and key order is
an item on every runtime (python builds a reply dict in one order, a port in
another, and a sorted dump makes the two look identical). Two of them cut error
bodies. One substituted a guessed id list when a step failed
(`parity_capture_store.py:485-487`), which is the worst of all: a capture that
invents data measures the harness.

The rules here are the §5.3 rules, in one place so a new family cannot opt out:

  * JSON at `indent=1`, `ensure_ascii=False`, INSERTION ORDER;
  * bytes inside a reply become `{__bytes_len__, __sha256__, __utf8__}` and the
    raw bytes go to a sidecar, so the body is both diffable and recoverable;
  * anything the HARNESS owns (a timestamp, a wait, a poll count) goes under a
    `harness` key, never mixed into the kernel's own answer — `diff.py` drops
    that key by schema instead of guessing which fields were ours;
  * a step that cannot proceed writes `{__harness_error__}` and the family
    fails. No substitute data, ever.
"""

from __future__ import annotations

import hashlib
import shutil
import json
from pathlib import Path

#: Fields the HARNESS owns. They are written under `harness`, and `diff.py`
#: drops that key. Listed here so the two cannot drift.
HARNESS_FIELDS = (
    "t",
    "t_send",
    "waited_s",
    "duration_s",
    "alive_s_before_sigterm",
    "port_open_after_s",
    "idle_after_s",
    "ps_polls",
    "request_numbers",
    "quiesce",
    # the memory-store censuses `store.memory_reflect` takes around its reflect;
    # `diff.py` N24 reads them before N14 drops them
    "flush_rows",
)


def _enc(s: str) -> bytes:
    """utf-8 with `surrogatepass`.

    A LONE SURROGATE IS AN ITEM (D13): python holds `\udcff` in a string and
    both ports have to say something about it. Writing with strict utf-8 raises
    instead of recording, so the capture died on the one case whose subject was
    that character. `surrogatepass` keeps the bytes; the diff sees them on every
    runtime and the encoder never decides the answer.
    """
    return s.encode("utf-8", "surrogatepass")


class HarnessError(Exception):
    """A step that cannot proceed. Recorded, then the family exits non-zero."""


def jsonable(x, sidecars: list | None = None, path: str = ""):
    """Bytes → a diffable stub plus (optionally) a raw sidecar entry."""
    if isinstance(x, (bytes, bytearray)):
        b = bytes(x)
        try:
            txt = b.decode("utf-8")
        except UnicodeDecodeError:
            txt = None
        if sidecars is not None:
            sidecars.append((path, b))
        return {
            "__bytes_len__": len(b),
            "__sha256__": hashlib.sha256(b).hexdigest(),
            "__utf8__": txt,
        }
    if isinstance(x, dict):
        return {k: jsonable(v, sidecars, f"{path}.{k}" if path else str(k)) for k, v in x.items()}
    if isinstance(x, (list, tuple)):
        return [jsonable(v, sidecars, f"{path}[{i}]") for i, v in enumerate(x)]
    return x


class Recorder:
    """One family's output directory."""

    def __init__(self, out: Path, family: str, runtime: str) -> None:
        self.out = out
        self.family = family
        self.runtime = runtime
        self.notes: list[dict] = []
        self.errors: list[dict] = []
        # WIPED, NOT MERGED. A re-run into the same directory that keeps whatever
        # the last one left makes the manifest a union of two runs: a file a
        # fixed family no longer writes stays, still carrying the old bytes, and
        # the diff reads it as agreement.
        if self.out.exists():
            shutil.rmtree(self.out)
        self.out.mkdir(parents=True)

    # ── primitives ───────────────────────────────────────────────────────────
    def _path(self, rel: str, suffix: str = ".json") -> Path:
        p = self.out / (rel + suffix)
        p.parent.mkdir(parents=True, exist_ok=True)
        return p

    def json(self, rel: str, obj, harness: dict | None = None) -> None:
        """One JSON document, insertion order kept, bytes side-carred."""
        sidecars: list = []
        body = jsonable(obj, sidecars)
        if harness:
            body = {"harness": jsonable(harness), "value": body} if not isinstance(body, dict) \
                else {**body, "harness": jsonable(harness)}
        self._path(rel).write_bytes(_enc(json.dumps(body, indent=1, ensure_ascii=False)))
        for field_path, raw in sidecars:
            name = field_path.replace("/", "_") or "bytes"
            self._path(f"{rel}__bytes/{name}", ".bin").write_bytes(raw)

    def raw(self, rel: str, data: bytes) -> None:
        p = self.out / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)

    def text(self, rel: str, s: str) -> None:
        self.raw(rel, _enc(s))

    def jsonl(self, rel: str, obj) -> None:
        p = self._path(rel, ".jsonl")
        with p.open("ab") as fh:
            fh.write(_enc(json.dumps(jsonable(obj), ensure_ascii=False) + "\n"))

    # ── the three recorded shapes ────────────────────────────────────────────
    def http(self, rel: str, exchanges: list[dict]) -> None:
        """Every REST call of a family, whole. Order is the order they were made."""
        self.json(rel, exchanges)

    def cli(self, rel: str, rec: dict, stdout: bytes, stderr: bytes) -> None:
        """A CLI case: raw stdout and stderr beside the parsed record.

        BYTES, not a decoded string: a runtime that emits invalid utf-8 on its
        way out (measured on the terminal and cli surfaces) is a difference, and
        `decode(errors='replace')` erases it.
        """
        self.json(rel, rec)
        self.raw(rel + ".stdout", stdout)
        self.raw(rel + ".stderr", stderr)

    def note(self, **kw) -> None:
        """Something the harness observed that is NOT the measurement. An empty
        `notes.json` is part of the CLEAN definition, so a note is a thing to go
        and fix, not a log line."""
        self.notes.append(kw)

    def harness_error(self, where: str, detail: str) -> dict:
        """A step that could not proceed. Recorded as data AND remembered, so
        `run.py capture` exits non-zero and the family is never read as clean."""
        d = {"__harness_error__": detail, "where": where}
        self.errors.append(d)
        return d

    # ── closing ──────────────────────────────────────────────────────────────
    def finish(self, meta: dict) -> dict:
        """`meta.json`, `notes.json` and the manifest. A file present for python
        and missing for a port is a DIFF LINE, which is why the manifest lists
        every file with its sha — a skip must never look like agreement."""
        self.json("meta", meta)
        self.json("notes", self.notes)
        if self.errors:
            self.json("harness_errors", self.errors)
        files = []
        for p in sorted(self.out.rglob("*")):
            if not p.is_file():
                continue
            rel = str(p.relative_to(self.out))
            if rel == "manifest.json":
                continue
            files.append({"path": rel, "sha256": hashlib.sha256(p.read_bytes()).hexdigest()})
        self._path("manifest").write_bytes(_enc(json.dumps(files, indent=1, ensure_ascii=False)))
        return {
            "family": self.family,
            "runtime": self.runtime,
            "files": len(files),
            "notes": len(self.notes),
            "harness_errors": len(self.errors),
        }
