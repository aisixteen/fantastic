"""backends/system_prompt_file — an AI agent's system block from a FILE. NO MODEL.

A scripted Responses endpoint (`runners/_responses_stub.py`: every turn answers
`done`, every request body recorded — the system block is its `instructions`) and,
on the PACKAGED host of `RUNTIME` (rust), every call through
`web_rest`: a `file_bridge` exposing `prompt.md`, its file view `pf` of that path,
and an AI agent whose record names `file_bridge_id` (the view) + `system_prompt_file`
(the view's path). The agent reads the file THROUGH the view, as itself, with the
view's own verbs only (its own `pf` line: `read`, `watch`; nothing keeps a list of
callers). Not set → nothing looked up, the default block; set and unusable →
the turn FAILS loud (`reason: invalid_meta`), the model is never called:

  1. the file is the block, its placeholders filled per agent (`{store}`), an
     unknown `{…}` kept verbatim;
  2. an editor's save (a direct write) is the block on a following turn — no
     restart — and `{self}` is this agent's identity;
  3. an atomic save (a temp file renamed over) is seen too;
  4. precedence: the record's `system_prompt` beats the file; null brings it back;
  5. `{self}\\n\\n{howto}` in the file == the default block, byte for byte;
  6. the file deleted → the turn fails naming the path (and reflect's
     `system_prompt_file_error` says it); written again → the file once more, the
     error gone;
  7. a view the agent holds no line on → the turn fails with the unknown answer
     for the view, nothing of the file reaches the model; the line written into
     the agent's `can` → the file reads (the view cannot block it);
  8. a path with no `file_bridge_id` → the turn fails; `file_bridge_id` alone →
     the default block;
  9. a malformed value is refused at the write.

Lines: the provider holds `memory` and `creds: get`, the chat `memory`, its `pf`
line and its provider line. A sibling `user_probe` holding only a `pf` line gets
the unknown answer from `creds`, and its `reflect` of `pf` lists only the verbs
its line names.
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
from helpers.lines import ALL, GET, PROVIDER, call, is_unknown
from helpers.verdict import verdict
from runners._responses_stub import ResponsesStub

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
KEY = "spf-e2e-" + secrets.token_hex(12)
KEY_VAR = "E2E_SPF_KEY"
#: the verbs the prompt loader sends a file view: its text and its changes
VIEW_VERBS = ["read", "watch"]
#: a file view's line on its bridge (the blueprint's `prompt_file` line)
VIEW_ON_BRIDGE = ["read", "watch_path", "unwatch_path"]
PATH = "prompt.md"
#: A sibling `user_probe` under `rest` holding only a `pf` line.
READER = "pf_reader"


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    stub = ResponsesStub(free_port(), [])
    stub.start()
    os.environ[KEY_VAR] = KEY  # the STORE reads it (env backend), never a record
    work = Path(tempfile.mkdtemp(prefix="e2e-system-prompt-file-"))
    prompt = work / "prompt.md"
    hk = HostKernel(spec, work).boot()
    root = hk.root_id
    bad: list[str] = []
    #: the chat's lines: its store, the prompt view (read + watch), its provider
    AI_CAN = {"memory": ALL, "pf": call(*VIEW_VERBS), "ai_provider": PROVIDER}

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def update(aid: str, **fields) -> dict:
        return hk.send(root, {"type": "update_agent", "id": aid, **fields})

    def via(src: str, target: str, payload: dict) -> dict:
        """`src` sends `payload` to `target` AS ITSELF; the onward result."""
        r = hk.send(src, {"type": "send", "target": target, "payload": payload})
        if "sent" not in r:
            return {"error": f"relay {src} refused: {str(r)[:200]}", "relay": True}
        return r.get("result") or {}

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

    def view_of(bridge: str, view: str, path: str) -> None:
        """A `file_bridge` exposing `path` and its file view `view` of that path, a child
        holding its one line on the bridge (the blueprint's `prompt_file` record)."""
        rec = hk.create(
            parent_id="rest",
            handler_module="file_bridge.tools",
            id=bridge,
            allow=[path],
            ingress_rule="allow_all_ingress",
        )
        need(rec.get("id") == bridge, f"create {bridge}: {str(rec)[:200]}")
        rec = hk.create(
            parent_id=bridge,
            handler_module="file_bridge.tools",
            id=view,
            view="file",
            bridge=bridge,
            path=path,
            display_name=path,
            can={"$parent": call(*VIEW_ON_BRIDGE)},
        )
        need(rec.get("id") == view, f"create {view}: {str(rec)[:200]}")

    def reflect_error() -> object:
        return hk.send("ai", {"type": "reflect"}).get("system_prompt_file_error")

    try:
        # the key's ref, pinned to the stub's host; the provider holds the `creds` line
        r = update(
            "creds",
            refs={"stub": {"host": f"127.0.0.1:{stub.port}", "env": {"api_key": KEY_VAR}}},
        )
        need(r.get("updated") is True, f"creds: the stub ref: {str(r)[:200]}")

        # the bridge exposes prompt.md; its file view `pf` of it reaches it by its one line
        prompt.write_text("ROLE-ONE store={store} {nope}\n")
        view_of("pf_fb", "pf", PATH)
        # a sibling holding only a `pf` line (written after pf exists): no line on the key store
        rec = hk.create(parent_id="rest", handler_module="user_probe.tools", id=READER,
                        can={"pf": call(*VIEW_VERBS)})
        need(rec.get("id") == READER, f"create {READER}: {str(rec)[:200]}")
        got = via(READER, "creds", {"type": "get", "ref": "stub"})
        need(is_unknown(got, "creds") and KEY not in str(got),
             f"{READER} holds no `creds` line: the store answers it unknown: {str(got)[:200]}")
        # its reflect of pf lists only the verbs its line names (`watch` is a file
        # view's own declared verb)
        got = via(READER, "pf", {"type": "reflect", "part": "docs"})
        shown = got.get("verbs") if isinstance(got.get("verbs"), dict) else {}
        need(set(shown) == set(VIEW_VERBS),
             f"{READER}'s reflect of pf lists only its line's verbs: {sorted(shown)} "
             f"({str(got)[:200] if not shown else ''})")
        # the provider (endpoint, key) and the chat `ai` naming it: the file and its view are the chat's
        rec = hk.create(
            parent_id="rest",
            handler_module="responses_backend.tools",
            id="ai_provider",
            endpoint=stub.url(),
            model="stub-model",
            credentials={"store": "creds", "ref": "stub"},
            memory_store_id="memory",
            can={"memory": ALL, "creds": GET},
        )
        need(rec.get("id") == "ai_provider", f"create ai_provider: {str(rec)[:200]}")
        rec = hk.create(
            parent_id="rest",
            handler_module="ai_chat.tools",
            id="ai",
            provider_id="ai_provider",
            model="stub-model",
            memory_store_id="memory",
            can=AI_CAN,
        )
        need(rec.get("id") == "ai", f"create ai: {str(rec)[:200]}")
        held = hk.send("ai", {"type": "reflect"}).get("can") or {}
        need(held.get("pf") == call(*VIEW_VERBS),
             f"ai's record holds exactly its pf line: {held.get('pf')!r}")

        # 9 · a malformed value is refused at the write
        for field, v in (
            ("system_prompt_file", ""),
            ("system_prompt_file", {"bridge": "pf", "path": "p"}),
            ("file_bridge_id", 5),
        ):
            r = update("ai", **{field: v})
            need(
                r.get("reason") == "bad_arg" and f"record field '{field}' must be" in str(r.get("error")),
                f"refused at the write: {field}={v!r} → {str(r)[:160]}",
            )

        default = turn()
        need("You are `ai`." in default, "nothing set: the default block")

        # 8 · a path with no file_bridge_id fails; file_bridge_id alone is the default
        update("ai", system_prompt_file=PATH)
        r = failed_turn()
        need(
            r.get("reason") == "invalid_meta" and "no file_bridge_id" in str(r.get("error")),
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
        save("{self}\n\n{howto}")
        layout = turn_until(lambda s: "You are `ai`." in s)
        update("ai", system_prompt_file=None)
        plain = turn()
        need(
            layout == plain and "You are `ai`." in plain,
            f"{{self}}\\n\\n{{howto}} == the default block "
            f"({len(layout)} vs {len(plain)} chars)",
        )
        update("ai", system_prompt_file=PATH)

        # 6 · deleted → the turn fails + the error; written again → the file
        save("BACK\n")
        turn_until(lambda s: s == "BACK\n")
        time.sleep(0.3)
        prompt.unlink()
        r = failed_until(lambda r: r.get("reason") == "invalid_meta")
        need(
            r.get("reason") == "invalid_meta"
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

        # 7 · a view the AI holds no line on: nothing of the file is read
        (work / "sealed.md").write_text("SECRET-ROLE\n")
        view_of("sealed_fb", "pf_sealed", "sealed.md")
        need("pf_sealed" not in (hk.send("ai", {"type": "reflect"}).get("can") or {}),
             "ai holds no line on pf_sealed")
        r = update("ai", file_bridge_id="pf_sealed", system_prompt_file="sealed.md")
        need(r.get("updated") is True, f"ai names pf_sealed: {str(r)[:200]}")
        r = failed_turn()
        need(
            r.get("reason") == "invalid_meta"
            and "no agent 'pf_sealed'" in str(r.get("error"))
            and "SECRET-ROLE" not in str(r),
            f"no line on the view: the turn fails with the unknown answer: {str(r)[:200]}",
        )
        # the line written into the AI's own `can`: the file reads (the view cannot block)
        r = update("ai", can={**AI_CAN, "pf_sealed": call(*VIEW_VERBS)})
        need(r.get("updated") is True, f"ai's can gains the pf_sealed line: {str(r)[:200]}")
        b = turn_until(lambda s: s == "SECRET-ROLE\n")
        need(b == "SECRET-ROLE\n", f"with the line, the sealed file is the block: {b[:80]!r}")
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
