"""backends/system_prompt_file — an AI agent's system block from a FILE. NO MODEL.

A scripted Responses endpoint (`runners/_responses_stub.py`: every turn answers
`done`, every request body recorded — the system block is its `instructions`) and,
on the PACKAGED host of `RUNTIME` (python | rust), every call through
`web_rest`: a `file_bridge` exposing `prompt.md` and an AI agent whose record names
`file_bridge_id` + `system_prompt_file` (a path on it). The agent reads the file
THROUGH the bridge, as itself (both grant halves explicit), and watches it with
`watch_path`. Not set → nothing looked up, the default block; set and unusable →
the turn FAILS loud (`reason: system_prompt_file`), the model is never called:

  1. the file is the block, its placeholders filled per agent (`{store}`), an
     unknown `{…}` kept verbatim;
  2. an editor's save (a direct write) is the block on a following turn — no
     restart — and `{self}` is this agent's identity;
  3. an atomic save (a temp file renamed over) is seen too;
  4. precedence: the record's `system_prompt` beats the file; null brings it back;
  5. `{tree}\\n\\n{self}\\n\\n{howto}` in the file == the default block, byte for byte;
  6. the file deleted → the turn fails naming the path (and reflect's
     `system_prompt_file_error` says it); written again → the file once more, the
     error gone;
  7. a bridge that does not admit the agent → the turn fails naming the refused
     verb, nothing of the file reaches the model;
  8. a path with no `file_bridge_id` → the turn fails; `file_bridge_id` alone →
     the default block;
  9. a malformed value is refused at the write.
"""

from __future__ import annotations

import os
import secrets
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, free_port, runtime
from helpers.verdict import verdict
from runners._responses_stub import ResponsesStub

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
KEY = "spf-e2e-" + secrets.token_hex(12)
KEY_VAR = "E2E_SPF_KEY"
FILE_VERBS = ["read_stream", "watch_path", "unwatch_path", "watch"]
PATH = "prompt.md"


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    stub = ResponsesStub(free_port(), [])
    stub.start()
    os.environ[KEY_VAR] = KEY  # the STORE reads it (env backend), never a record
    work = Path(tempfile.mkdtemp(prefix="e2e-system-prompt-file-"))
    prompt = work / "prompt.md"
    hk = HostKernel(spec, work, reach=["ai", "pf", "pf_sealed"]).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def update(aid: str, **fields) -> dict:
        return hk.send(root, {"type": "update_agent", "id": aid, **fields})

    def callable_by(aid: str) -> dict:
        r = hk.send("kernel", {"type": "list_agents"})
        rec = next(a for a in r.get("agents") or [] if a.get("id") == aid)
        return dict(rec.get("callable_by") or {})

    def turn() -> str:
        """One `send`; the system block the model got for it."""
        before = len(stub.requests)
        hk.send("ai", {"type": "send", "text": "go"})
        for _ in range(300):
            if len(stub.requests) > before:
                break
            time.sleep(0.05)
        if len(stub.requests) <= before:
            return "<no request>"
        return str(stub.requests[-1]["body"].get("instructions"))

    def turn_until(pred, tries: int = 12) -> str:
        """Turns until `pred(block)` (the watcher marks the text stale; a turn
        after that reads it again)."""
        got = ""
        for _ in range(tries):
            got = turn()
            if pred(got):
                break
            time.sleep(0.25)
        return got

    def save(text: str) -> None:
        time.sleep(0.3)  # one save per watcher burst
        prompt.write_text(text)

    def failed_turn() -> dict:
        """A turn that must fail loud: its reply names why, no model request."""
        before = len(stub.requests)
        r = hk.send("ai", {"type": "send", "text": "go"})
        time.sleep(0.3)
        need(len(stub.requests) == before, "the model was not called on another block")
        return r if isinstance(r, dict) else {}

    def failed_until(pred, tries: int = 12) -> dict:
        r: dict = {}
        for _ in range(tries):
            r = failed_turn_quiet()
            if pred(r):
                break
            time.sleep(0.25)
        return r

    def failed_turn_quiet() -> dict:
        before = len(stub.requests)
        r = hk.send("ai", {"type": "send", "text": "go"})
        r = r if isinstance(r, dict) else {}
        r["_called"] = len(stub.requests) > before
        return r

    def reflect_error() -> object:
        return hk.send("ai", {"type": "reflect"}).get("system_prompt_file_error")

    try:
        # the key's ref, pinned to the stub's host; creds and memory accept the AI
        r = update(
            "creds",
            refs={"stub": {"host": f"127.0.0.1:{stub.port}", "env": {"api_key": KEY_VAR}}},
            callable_by={**callable_by("creds"), "ai": ["get"]},
        )
        need(r.get("updated") is True, f"creds: the stub ref + ai: {str(r)[:200]}")
        r = update("memory", callable_by={**callable_by("memory"), "ai": "*"})
        need(r.get("updated") is True, f"memory accepts ai: {str(r)[:200]}")

        # the bridge: exposes prompt.md, admits the AI for the read + watch verbs
        prompt.write_text("ROLE-ONE store={store} {nope}\n")
        rec = hk.create(
            parent_id="rest",
            handler_module="file_bridge.tools",
            id="pf",
            allow=["prompt.md"],
            ingress_rule="allow_all_ingress",
            callable_by={"$self": "*", "$parent": "*", "ai": FILE_VERBS},
        )
        need(rec.get("id") == "pf", f"create pf: {str(rec)[:200]}")
        rec = hk.create(
            parent_id="rest",
            handler_module="responses_backend.tools",
            id="ai",
            endpoint=stub.url(),
            model="stub-model",
            credentials={"store": "creds", "ref": "stub"},
            memory_store_id="memory",
            can_call={
                "$self": "*",
                "memory": "*",
                "creds": ["get"],
                root: ["reflect"],
                "pf": FILE_VERBS,
            },
            callable_by={"$self": "*", "$parent": "*", "rest": "*"},
        )
        need(rec.get("id") == "ai", f"create ai: {str(rec)[:200]}")

        # 9 · a malformed value is refused at the write
        for field, v in (
            ("system_prompt_file", ""),
            ("system_prompt_file", {"bridge": "pf", "path": "p"}),
            ("file_bridge_id", 5),
        ):
            r = update("ai", **{field: v})
            need(
                f"{field} must be a non-empty" in str(r.get("error")),
                f"refused at the write: {field}={v!r} → {str(r)[:160]}",
            )

        default = turn()
        need("You are `ai`." in default, "nothing set: the default block")

        # 8 · a path with no file_bridge_id fails; file_bridge_id alone is the default
        update("ai", system_prompt_file=PATH)
        r = failed_turn()
        need(
            r.get("reason") == "system_prompt_file" and "no file_bridge_id" in str(r.get("error")),
            f"a path without file_bridge_id fails loud: {str(r)[:200]}",
        )
        update("ai", system_prompt_file=None)
        update("ai", file_bridge_id="pf")
        b = turn()
        need("You are `ai`." in b, "file_bridge_id alone: the default block")
        r = update("ai", system_prompt_file=PATH)
        need(r.get("updated") is True, f"system_prompt_file set: {str(r)[:200]}")

        # 1 · the file is the block; {store} filled, an unknown {…} kept
        b = turn()
        need(b == "ROLE-ONE store=memory {nope}\n", f"the file is the block: {b[:120]!r}")
        need(reflect_error() is None, "no system_prompt_file_error while it reads")

        # 2 · an editor's save, live on a following turn; {self} filled
        save("ROLE-TWO {self}\n")
        b = turn_until(lambda s: s.startswith("ROLE-TWO"))
        need(b.startswith("ROLE-TWO You are `ai`."), f"an editor's save is live: {b[:120]!r}")

        # 3 · an atomic save (temp + rename over)
        time.sleep(0.3)
        tmp = work / ".prompt.md.tmp"
        tmp.write_text("ROLE-THREE\n")
        os.replace(tmp, prompt)
        b = turn_until(lambda s: s == "ROLE-THREE\n")
        need(b == "ROLE-THREE\n", f"an atomic save is live: {b[:120]!r}")

        # 4 · precedence: record > file; null brings the file back
        update("ai", system_prompt="RECORD")
        b = turn()
        need(b == "RECORD", f"the record's system_prompt beats the file: {b[:80]!r}")
        update("ai", system_prompt=None)
        b = turn()
        need(b == "ROLE-THREE\n", f"null: the file again: {b[:80]!r}")

        # 5 · the default layout in placeholders IS the default block
        save("{tree}\n\n{self}\n\n{howto}")
        layout = turn_until(lambda s: "You are `ai`." in s)
        update("ai", system_prompt_file=None)
        plain = turn()
        need(
            layout == plain and "You are `ai`." in plain,
            f"{{tree}}\\n\\n{{self}}\\n\\n{{howto}} == the default block "
            f"({len(layout)} vs {len(plain)} chars)",
        )
        update("ai", system_prompt_file=PATH)

        # 6 · deleted → the turn fails + the error; written again → the file
        save("BACK\n")
        turn_until(lambda s: s == "BACK\n")
        time.sleep(0.3)
        prompt.unlink()
        r = failed_until(lambda r: r.get("reason") == "system_prompt_file")
        need(
            r.get("reason") == "system_prompt_file"
            and "prompt.md" in str(r.get("error"))
            and not r.get("_called"),
            f"the file deleted: the turn fails loud: {str(r)[:200]}",
        )
        why = reflect_error()
        need("prompt.md" in str(why), f"reflect names why: {why!r}")
        save("AGAIN\n")
        b = turn_until(lambda s: s == "AGAIN\n")
        need(b == "AGAIN\n", f"written again: the file once more: {b[:80]!r}")
        need(reflect_error() is None, "the error is gone once a read succeeds")

        # 7 · a bridge that does not admit the AI: nothing of the file is read
        (work / "sealed.md").write_text("SECRET-ROLE\n")
        hk.create(
            parent_id="rest",
            handler_module="file_bridge.tools",
            id="pf_sealed",
            allow=["sealed.md"],
            ingress_rule="allow_all_ingress",
            callable_by={"$self": "*", "$parent": "*"},
        )
        cc = {"$self": "*", "memory": "*", "creds": ["get"], root: ["reflect"],
              "pf": FILE_VERBS, "pf_sealed": FILE_VERBS}
        update("ai", can_call=cc, file_bridge_id="pf_sealed", system_prompt_file="sealed.md")
        r = failed_turn()
        need(
            r.get("reason") == "system_prompt_file"
            and "watch_path" in str(r.get("error"))
            and "SECRET-ROLE" not in str(r),
            f"an ungranted bridge: the turn fails naming the refused verb: {str(r)[:200]}",
        )
    finally:
        hk.close()
        stub.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(
        not bad,
        f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""),
    )
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
