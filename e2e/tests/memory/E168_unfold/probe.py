"""memory/unfold — the model must UNFOLD a folded reply to pass. LLM-driven, Rust lane.

One AI chat and one file agent (`notes`) holding `big.txt`: 100 lines of 800 characters,
so its `read` reply is over the autofold size (16 000 tokens) and folds once it is
answered. Line 80 is `KEY=<token>`, minted now: neither the marker's head (the body's
first line) nor anything the model was told carries it.

  1. "Read big.txt. Answer only ok." — the reply arrives whole and folds after the answer.
  2. An unrelated small task — one more answered turn, so the fold is in force.
  3. "What is the token on the line that starts with KEY= in big.txt? Do not read the file
     again." — the body is only in the store: the model must send itself `recall` with the
     folded row's id.

Scored from the store (the rows and each answer row's `attributes.parts`, the bytes and
form of every part of the request it answered):
  * the first request of message 3 shows the read row reduced (`anchor`), no larger than a
    marker, and no part of it holds the body;
  * the reduced row is the fold marker: no view array stubs it (a strip stub is `res: stub`);
  * after message 3 a `recall` the model wrote names the read row's id, the reply to that
    recall holds the token, and the answer right after it does;
  * no call of any verb reaches `notes` after message 1 (no read, grep or list);
  * no answer of the run opens with a time (`[2026-…`), `[thought` or a marker opener.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import secrets
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, human, human_line, lines, verdict
from helpers.kernel import home, open_kernel
from helpers.rows import rows_for

BK = _resolve_backend()
FILES = "notes"
FILE = "big.txt"
#: a line of the body, 800 characters with its newline
LINE = 795
#: the lines of the file, and the one that holds the key
LINES, KEY_AT = 100, 80

#: what an answer of the model's own must never open with: a time head, the old thought
#: anchor, or a marker of either type (map-once)
COPIED = re.compile(
    r"^\s*(\[\d{4}-\d\d-\d\d|\[thought|<folded|<thought|<stripped|<image |<tool_response>"
    r'|\{"folded"|\{"thought"|\{"stripped"|\{"image")'
)


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


def body(token: str) -> str:
    out = []
    for n in range(1, LINES + 1):
        out.append(f"KEY={token}" if n == KEY_AT else f"L{n:03d} " + "x" * LINE)
    return "\n".join(out) + "\n"


def idents(lst: list[dict]) -> list[dict]:
    """Each entry's identity: `{part, ref}` for a row, `{part, n?}` for a system piece."""
    seen: dict[str, int] = {}
    out = []
    for e in lst:
        i = {"part": e.get("part")}
        if "ref" in e:
            i["ref"] = e["ref"]
        else:
            k = json.dumps(e.get("part"))
            c = seen.get(k, 0)
            if c > 0:
                i["n"] = c
            seen[k] = c + 1
        out.append(i)
    return out


def apply(prev: list[dict], parts: dict) -> list[dict]:
    """The full parts list `parts` stands for (a checkpoint, or a delta against `prev`):
    the ai core's `row_attrs::apply`."""
    if parts.get("full") is True:
        return list(parts.get("list") or [])
    drops = parts.get("drop") or []
    changes = parts.get("change") or []
    out = []
    for e, i in zip(prev, idents(prev)):
        if i in drops:
            continue
        e = dict(e)
        for c in changes:
            if {k: v for k, v in c.items() if k in ("part", "ref", "n")} == i:
                for f in ("form", "hash", "bytes"):
                    if f in c:
                        e[f] = c[f]
        out.append(e)
    for a in parts.get("add") or []:
        e = {k: v for k, v in a.items() if k != "at"}
        out.insert(min(int(a.get("at") or 0), len(out)), e)
    return out


def requests(rows: list[dict]) -> dict[int, list[dict]]:
    """Each answer row's id -> the full parts list of the request it answered."""
    out: dict[int, list[dict]] = {}
    prev: list[dict] = []
    for r in rows:
        parts = (r.get("attributes") or {}).get("parts")
        if r.get("role") != "assistant" or not parts:
            continue
        prev = apply(prev, parts)
        out[r["id"]] = prev
    return out


def calls(content: str, verb: str) -> list[str]:
    """The `<call name="send">…</call>` envelopes of a stored answer whose verb is `verb`."""
    return [c for c in re.findall(r'<call\s+name="send">.*?</call>', content, re.DOTALL)
            if re.search(rf"<type>\s*{verb}\s*</type>", c)]


def first_user(rows: list[dict], text: str) -> int:
    """The index of the user row that delivered `text` (a sender line may open it)."""
    for i, r in enumerate(rows):
        if r.get("role") == "user" and text in str(r.get("content")):
            return i
    raise SystemExit(verdict(False, f"no row for the message {text!r}"))


async def main():
    token = "zk_" + secrets.token_hex(6)
    root, core, close = await open_kernel(prefix="mem_unfold_", reach=[FILES, "ai", "user"])
    try:
        Path(FILE).write_text(body(token))
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": home(root), "id": FILES,
            "handler_module": "file_bridge.tools", "allow": ["."], "root": ".",
            "ingress_rule": "allow_all_ingress", "description": "This run's files.",
        })
        assert not r.get("error"), f"create {FILES}: {r}"
        await human(root, ai=[])
        conn = await connect(
            root, core, BK,
            can={FILES: lines.ALL, "user": lines.ALL},
            limits=Limits.from_env(os.environ),
            record={"file_bridge_id": FILES},
        )
        await human_line(root, conn.id)

        m1 = f"Read `{FILE}` through `{FILES}`. Answer only `ok`."
        m2 = "What is 17 times 23? Answer with the number only."
        m3 = (f"What is the token on the line that starts with `KEY=` in `{FILE}`? "
              f"Do not read the file again. Answer with the token only.")
        for k, text in enumerate((m1, m2, m3), start=1):
            cut = await drive(conn, text, client_id="unfold", via="user")
            print(f"  message {k}: {'cut: ' + cut if cut else 'ended'}", flush=True)
        await conn.close()

        every = await rows_for(root, conn.id)
        rows = [r for r in every if r.get("kind") not in ("view", "keep")]
        # the view arrays the projections wrote: a strip stub of a row is `res: stub` there
        stubbed = {e.get("id") for r in every if r.get("kind") == "view"
                   for e in r.get("ids") or [] if e.get("res") == "stub"}
        i1, i2, i3 = (first_user(rows, t) for t in (m1, m2, m3))
        size = len(body(token))
        read_rows = [r for r in rows[i1:i2]
                     if r.get("role") == "user" and FILES in str(r.get("content"))[:200]
                     and len(str(r.get("content"))) > size // 2]
        read_id = read_rows[0]["id"] if read_rows else None
        answers = [r for r in rows if r.get("role") == "assistant"]
        after3 = [r for r in rows[i3 + 1:] if r.get("role") == "assistant"]
        req = requests(rows)
        first3 = req.get(after3[0]["id"]) if after3 else None
        held = [p for p in first3 or [] if p.get("ref") == read_id]
        recalls = [c for r in after3 for c in calls(str(r.get("content")), "recall")]
        # after message 1 the file agent is reached by no call at all: no read, grep, list
        to_files = [c for r in rows[i2:] if r.get("role") == "assistant"
                    for c in re.findall(r'<call\s+name="send">.*?</call>',
                                        str(r.get("content")), re.DOTALL)
                    if re.search(rf"<target_id>\s*{FILES}\s*</target_id>", c)]
        # THE TOKEN CAME BY THE RECALL: the answer that recalls the read row by its id,
        # then the reply to that recall holds the token, then the next answer does
        unfolded = False
        tail = rows[i3 + 1:]
        for j, r in enumerate(tail):
            if r.get("role") != "assistant" or read_id is None or not any(
                    re.search(rf"<id>\s*{read_id}\s*</id>", c)
                    for c in calls(str(r.get("content")), "recall")):
                continue
            reply = next((x for x in tail[j + 1:] if x.get("role") == "user"), None)
            answer = next((x for x in tail[j + 1:] if x.get("role") == "assistant"), None)
            unfolded = (reply is not None and " recall" in str(reply.get("content"))[:200]
                        and token in str(reply.get("content"))
                        and answer is not None and token in str(answer.get("content")))
            break
        copied = [str(r.get("content"))[:80] for r in answers
                  if COPIED.match(str(r.get("content")))]
        print(f"     read row={read_id}  request 3's part of it={held}  "
              f"recalls after 3={len(recalls)}  calls to {FILES} after 1={len(to_files)}",
              flush=True)

        checks = [
            ("message 1's read reply is stored whole", read_id is not None),
            ("the first request of message 3 shows the read row reduced, no larger than a "
             "marker",
             len(held) == 1 and held[0].get("form") == "anchor"
             and int(held[0].get("bytes") or 0) < 2000),
            ("that reduced row is the fold marker, not a strip stub (no view array stubs it)",
             read_id is not None and read_id not in stubbed),
            ("no part of that request holds the body",
             first3 is not None and all(int(p.get("bytes") or 0) < size // 2 for p in first3)),
            ("after message 3 the model recalls the read row by its id",
             read_id is not None
             and any(re.search(rf"<id>\s*{read_id}\s*</id>", c) for c in recalls)),
            ("the token came by that recall: its reply holds it, and the answer after it",
             unfolded),
            (f"no call reaches {FILES} after message 1 (no read, grep or list)", not to_files),
            (f"no answer opens with a time or a marker: {copied[:2]}", not copied),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}", flush=True)
        allok = all(ok for _, ok in checks)
        verdict(allok, "unfolded by recall, the token from the stored row"
                if allok else "an unfold check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
