"""E62 mcp_server — ONE remote MCP server as a host agent, from the canvas:

a) the double-click's `mcp connector` tile places the add form;
b) url + key → `add_mcp` on the canvas's node: an `mcp_client.tools` agent under
   `canvas_root`, naming its key's ref — the key itself on no record (it is in
   the user's credentials file, bound to the server's host);
c) the form gives way to the agent's view: its url, the server, its TOOLS;
d) a tool answers through the agent (`fantastic send`), and the server saw the
   key as `Authorization: Bearer` on EVERY request;
e) ⟳ lists again (`boot`): the server is asked for its tools once more.

The server is the lib's fake (E95: protocol 2026-07-28, it refuses a request
without the key)."""

from __future__ import annotations

import asyncio
import importlib.util
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, private_write, spot, keys_dir  # noqa: E402

E95 = Path(__file__).resolve().parents[6] / "e2e" / "tests" / "connectors" / "E95_mcp_client" / "probe.py"


def fake_server():
    sys.path.insert(0, str(E95.parents[3]))  # e2e/, for its helpers
    spec = importlib.util.spec_from_file_location("e95_probe", E95)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.Fake(), mod.KEY


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    # where keys are kept: the user's choice (plain), as a set-up leaves it
    private_write(keys_dir(rig.home) / "mode", "plain")
    fake, key = fake_server()
    rig.init()
    from _lib.dev_surface import frame_eval  # noqa: F401

    r: dict = {}
    info: dict = {}
    secs = rig.limits["seconds"]

    def send(target: str, payload: dict) -> dict:
        p = subprocess.run([str(rig.binary), "send", json.dumps({"target": target, "payload": payload})],
                           cwd=rig.proj, env=rig.env(), capture_output=True, text=True, timeout=60)
        try:
            return json.loads(p.stdout).get("result") or {}
        except ValueError:
            return {"error": (p.stdout + p.stderr)[-300:]}

    async def drive() -> None:
        async with rig.page(1600, 1000) as pg:
            await asyncio.sleep(1.5)
            # a) the tile → the form
            await spot(pg, 400, 300, "mcp connector")
            r["form"] = bool(await pg.until("!!document.querySelector('.agent-frame .mcp-add input[name=url]')", 10))
            # b) url + key → add
            await pg.js(f"(()=>{{const f=document.querySelector('.mcp-add'); f.querySelector('input[name=url]').value={json.dumps(fake.url)}; f.querySelector('input[name=key]').value={json.dumps(key)}; f.requestSubmit()}})()")
            # c) its view: url, server, tools
            r["view"] = bool(await pg.until("!!document.querySelector('.agent-frame .mcp-view .tool')", secs))
            info["form_note"] = await pg.js("document.querySelector('.mcp-add .err')?.textContent || ''")
            text = await pg.js("document.querySelector('.agent-frame .mcp-view')?.innerText || ''") or ""
            info["view"] = text[:400]
            r["view_url"] = fake.url in text
            r["view_server"] = "e2e-fake" in text
            r["view_tools"] = "get_weather" in text and "reflect —" not in text
            r["form_gone"] = not await pg.js("!!document.querySelector('.mcp-add')")
            # e) ⟳ lists again
            before = len(fake.calls("tools/list"))
            await pg.js("[...document.querySelectorAll('.mcp-view h4 button')][0]?.click()")
            for _ in range(secs * 2):
                if len(fake.calls("tools/list")) > before:
                    break
                await asyncio.sleep(0.5)
            r["relisted"] = len(fake.calls("tools/list")) > before

    try:
        asyncio.run(drive())
        # b) the agent, its ref, no key on any record
        # the connector agents, under the canvas's node (not the `mcp` leg's own bridge)
        recs = [p for p in (rig.proj / ".fantastic" / "agents").rglob("mcp_*/agent.yaml") if "canvas_root/agents/" in str(p)]
        info["agents"] = [str(p.relative_to(rig.proj)) for p in recs]
        import yaml
        rec = yaml.safe_load(recs[0].read_text()) if recs else {}
        aid = rec.get("id", "")
        r["agent_under_canvas"] = bool(recs) and "canvas_root/agents/" in str(recs[0])
        r["agent_names_ref"] = (rec.get("credentials") or {}).get("ref", "").startswith("mcp_client/127.0.0.1:")
        leaked = [str(p) for p in (rig.proj / ".fantastic").rglob("*.yaml") if key in p.read_text(errors="ignore")]
        r["key_on_no_record"] = not leaked
        r["key_in_home"] = key in (keys_dir(rig.home) / "credentials.yaml").read_text() if (keys_dir(rig.home) / "credentials.yaml").exists() else False
        # d) a tool answers through the agent
        ans = send(aid, {"type": "get_weather", "city": "Kyiv"})
        info["answer"] = json.dumps(ans)[:300]
        r["tool_answers"] = "sunny in Kyiv" in json.dumps(ans)
        r["bearer_every_time"] = bool(fake.seen) and all(h.get("authorization") == f"Bearer {key}" for h, _ in fake.seen)
    finally:
        rig.stop()
        fake.srv.shutdown()
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
