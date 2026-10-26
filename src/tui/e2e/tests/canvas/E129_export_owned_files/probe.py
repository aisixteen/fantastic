"""E129 export_owned_files — a subtree with owned files, exported and imported (#268).

The E125 team, with files an agent owns: `mid` has a `system_prompt_file`
(`prompts/mid.md`) and an html view (`view.bytes`); `leaf` runs on a FOLDER BUNDLE
(`.fantastic/bundles/e129_card/` with `bundle.yaml`, `agent.py`, `panel.html`).

a) `user export_subtree` writes, beside `blueprint.yaml`, `mid/directive.md`,
   `mid/view.html` and `bundles/e129_card/{bundle.yaml,agent.py,panel.html}`, each
   byte for byte its source; the blueprint's header lists every code file as
   `# code: <path>` (the view and the bundle files, not the directive); the
   record holds `view.bytes_file` and no `bytes`, and `system_prompt_file` points
   at the copy; the folder still passes the secrets / runtime / row checks;
b) the kernel root's export gives the same files and blueprint;
c) importing without `allow_code` is refused (`code_not_allowed`, the code named)
   and creates nothing; with `allow_code` and `approve` the team is made under
   `newparent`, the export folder's files are untouched, and the new records keep
   the pointers (`view.bytes_file`, `system_prompt_file`).

(The importer does not write the owned files into the new agents' folders: that
is documented as "not yet" in docs/system.md, so the files compared are the
export folder's.)"""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, verdict  # noqa: E402
from _lib.export_live import LEAF_AT, SECRET, ROW, TEAM, check_folder, team_agents  # noqa: E402

BUNDLE = "e129_card"
BUNDLE_FILES = {
    "bundle.yaml": "base: py_agent\nsentence: A card (E129).\ncommand: [python3, agent.py]\nverbs:\n  hello: \"args: none.\"\nrecord:\n  inherit_env: [PATH]\n",
    "agent.py": "import fantastic_agent as fa\n\n\ndef hello(p):\n    return {'greeting': 'E129 hello é'}\n\n\nfa.serve({'hello': hello})\n",
    "panel.html": "<h3>E129 panel</h3>\n<button id=h>hello</button>\n",
}
DIRECTIVE = "You are the middle of the E129 team.\nAnswer briefly.\n"
VIEW = "<div style='color:#cde'>E129 view é</div>\n"


def bind_of(ports: list[str]) -> dict[str, str]:
    return {p: p.split(":", 1)[-1].lstrip("@") for p in ports}


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "out").mkdir()
    (rig.proj / "prompts").mkdir()
    (rig.proj / "prompts" / "mid.md").write_text(DIRECTIVE)
    agents = team_agents(extra={
        "mid": {"system_prompt_file": "prompts/mid.md", "view": {"kind": "html", "bytes": VIEW, "calls": ["lead"]}},
    })
    # `leaf` runs on the folder bundle
    agents = [(p, rec) for p, rec in agents if rec["id"] != "leaf"]
    agents.append((LEAF_AT, {"id": "leaf", "handler_module": BUNDLE, "display_name": "leaf", "callable_by": {"mid": "*"},
                             "api_key": SECRET, "guid": "guid-leaf", "history": [ROW], "rows": [{"text": ROW}]}))
    r: dict = {}
    info: dict = {}
    # the project is made by a first boot; the bundle and the team are written after it, host stopped
    rig.init()
    rig.stop()
    bdir = rig.proj / ".fantastic" / "bundles" / BUNDLE
    bdir.mkdir(parents=True)
    for n, t in BUNDLE_FILES.items():
        (bdir / n).write_text(t)
    for parent, rec in agents:
        rig.seed_host_agent(parent, rec)
    rig.start()

    async def run() -> None:
        from canvas import host_call
        user = lambda payload: host_call(rig.port, "user", payload)  # noqa: E731
        root = lambda payload: host_call(rig.port, "kernel", payload)  # noqa: E731
        out = rig.proj / "out" / "owned"
        a = await user({"type": "export_subtree", "agent_id": "lead", "path": "out/owned"})
        info["export_reply"] = a
        r["a1 the export reports three agents"] = a.get("exported") == 3 and "error" not in a
        want = {
            "mid/directive.md": DIRECTIVE, "mid/view.html": VIEW,
            **{f"bundles/{BUNDLE}/{n}": t for n, t in BUNDLE_FILES.items()},
        }
        for rel, text in want.items():
            f = out / rel
            r[f"a2 {rel} is there, byte for byte its source"] = f.is_file() and f.read_bytes() == text.encode()
        r["a3 the reply lists the files"] = {"blueprint.yaml", *want} <= set(a.get("files") or [])
        text = (out / "blueprint.yaml").read_text() if (out / "blueprint.yaml").is_file() else ""
        header = [ln for ln in text.splitlines() if ln.startswith("#")]
        info["header"] = header
        code = [ln.removeprefix("# code: ") for ln in header if ln.startswith("# code: ")]
        r["a4 the header marks every code file"] = set(code) == {"mid/view.html", *(f"bundles/{BUNDLE}/{n}" for n in BUNDLE_FILES)}
        r["a5 and not the directive"] = "mid/directive.md" not in code
        import yaml
        doc = yaml.safe_load(text) or {}
        mid = doc["agents"][0]["agents"][0] if doc.get("agents") else {}
        r["a6 the view record holds bytes_file, no bytes"] = (mid.get("view") or {}).get("bytes_file") == "mid/view.html" and "bytes" not in (mid.get("view") or {})
        r["a7 the view keeps its kind and calls"] = (mid.get("view") or {}).get("kind") == "html" and (mid.get("view") or {}).get("calls") == ["lead"]
        r["a8 system_prompt_file points at the copy"] = mid.get("system_prompt_file") == "mid/directive.md"
        r["a9 the leaf's handler is the bundle"] = ((mid.get("agents") or [{}])[0]).get("handler_module") == BUNDLE
        for k, v in check_folder(out, rig.proj).items():
            r[f"a10 folder: {k}"] = v
        # b) through the root: the same files
        b = await root({"type": "export_subtree", "agent_id": "lead", "path": "out/owned_root"})
        info["root_reply"] = b
        o2 = rig.proj / "out" / "owned_root"
        r["b1 the root's folder holds the same files, byte for byte"] = (
            "error" not in b and sorted(p.relative_to(o2).as_posix() for p in o2.rglob("*") if p.is_file())
            == sorted(p.relative_to(out).as_posix() for p in out.rglob("*") if p.is_file())
            and all((o2 / p.relative_to(out)).read_bytes() == p.read_bytes() for p in out.rglob("*") if p.is_file()))
        # c) import
        bind = bind_of(a.get("ports") or [])
        refused = await user({"type": "import_subtree", "path": "out/owned", "parent_id": "newparent", "bind": bind, "approve": True})
        info["refused"] = refused
        r["c1 without allow_code the import is refused"] = refused.get("reason") == "code_not_allowed"
        r["c2 and the refusal names the code files"] = "mid/view.html" in str(refused.get("error")) and f"bundles/{BUNDLE}/agent.py" in str(refused.get("error"))
        await asyncio.sleep(1.0)
        base = rig.proj / ".fantastic" / FOLDER / "agents" / "newparent" / "agents"
        r["c3 and nothing is created"] = not base.exists() or not any(base.iterdir())
        done = await user({"type": "import_subtree", "path": "out/owned", "parent_id": "newparent", "bind": bind, "allow_code": True, "approve": True})
        info["import_reply"] = done
        ids = done.get("ids") or {}
        r["c4 with allow_code and approve three agents are made"] = done.get("imported") == 3 and set(ids) == set(TEAM)
        rec: dict = {}
        end = time.time() + 20
        while time.time() < end and not rec:
            rec = rig.host_record(ids.get("mid", "?")) or {}
            await asyncio.sleep(0.25)
        info["new_mid"] = rec
        r["c5 the new mid keeps the pointers"] = (rec.get("view") or {}).get("bytes_file") == "mid/view.html" and rec.get("system_prompt_file") == "mid/directive.md"
        leaf = rig.host_record(ids.get("leaf", "?")) or {}
        r["c6 the new leaf runs on the bundle"] = leaf.get("handler_module") == BUNDLE
        r["c7 the export folder's files are untouched"] = all((out / rel).is_file() and (out / rel).read_bytes() == t.encode() for rel, t in want.items())
        r["c8 the sources are untouched"] = (rig.proj / "prompts" / "mid.md").read_text() == DIRECTIVE and all((bdir / n).read_text() == t for n, t in BUNDLE_FILES.items())

    try:
        asyncio.run(run())
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:4000])
    bad = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not bad, f"{len(r) - len(bad)}/{len(r)} pass" + ("" if not bad else f" — failed: {'; '.join(bad)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
