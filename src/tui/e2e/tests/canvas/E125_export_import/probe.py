"""E125 export_import — export a subtree through a real kernel, import it back (#268).

A real host (`fantastic canvas`), a seeded three-level team of key-free agents
(`lead` > `mid` > `leaf`, exec bridges) that holds a key value, runtime fields and
row text, and that reaches the project's `files` agent from outside the subtree.

a) `user export_subtree {agent_id: lead, path: out/team}` writes a blueprint folder:
   three agents nested (no `parent_id`), the inside grant by id, the outside grant a
   declared port `@files`, and no key, runtime field or row text in any file;
   the reply says `exported: 3` and lists the port; exporting into the same
   (now non-empty) folder again is refused (`exists`);
b) `import_subtree` under `newparent` without `approve` asks for approval and creates
   nothing; with `approve` and the port bound to `files` it makes three agents
   under `newparent` with fresh ids, records equal to the originals (what a
   blueprint carries), the inside grant on the new ids and the port grant on `files`.

Every call is `host_call`, the way a canvas tab reaches the host."""

from __future__ import annotations

import asyncio
import json
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, verdict  # noqa: E402
from _lib.export_live import OUTSIDE, TEAM, check_folder, strip, team_agents  # noqa: E402


def mapped(rec: dict, ids: dict[str, str]) -> dict:
    """An original record as the import should leave it: its inside grants on the new ids; the outside
    ones (the ports) were bound back to the agents they stood for, so they are what they were."""
    out = strip(rec)
    for g in ("can_call", "callable_by"):
        if g in out:
            out[g] = {ids.get(k, k): v for k, v in out[g].items()}
    return out


def bind_of(ports: list[str]) -> dict[str, str]:
    """Every declared port bound to the agent it stood for: `@files` -> files, `@connector:user` -> user."""
    return {p: p.split(":", 1)[-1].lstrip("@") for p in ports}


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "out").mkdir()
    r: dict = {}
    info: dict = {}
    rig.init(host_agents=team_agents())
    # the team as the booted host holds it (it adds its baseline grants): what the export is compared with
    # The baseline grants reach the disk on the loader's debounce (a slow runner: seconds):
    # read the records once every one of them carries `$self`, or the "originals" are the seed.
    orig: dict = {}
    end = time.time() + 30
    while time.time() < end:
        orig = {i: rig.host_record(i) or {} for i in TEAM}
        if all("$self" in (rec.get("can_call") or {}) for rec in orig.values()):
            break
        time.sleep(0.25)

    async def run() -> None:
        from canvas import host_call
        user = lambda payload: host_call(rig.port, "user", payload)  # noqa: E731
        # a) export
        out = rig.proj / "out" / "team"
        a = await user({"type": "export_subtree", "agent_id": "lead", "path": "out/team"})
        info["export_reply"] = a
        r["a1 the export reports three agents"] = a.get("exported") == 3 and "error" not in a
        ports = a.get("ports") or []
        bind = bind_of(ports)
        info["bind"] = bind
        r["a2 the reply lists the port @files, the rest connectors"] = f"@{OUTSIDE}" in ports and all(p.startswith("@connector:") for p in ports if p != f"@{OUTSIDE}")
        for k, v in check_folder(out, rig.proj).items():
            r[f"a3 folder: {k}"] = v
        again = await user({"type": "export_subtree", "agent_id": "lead", "path": "out/team"})
        info["again"] = again
        r["a4 exporting into the non-empty folder is refused"] = again.get("reason") == "exists"
        # b) import
        sub = f"{FOLDER}/agents/newparent/agents"
        base = rig.proj / ".fantastic" / sub
        plan = await user({"type": "import_subtree", "path": "out/team", "parent_id": "newparent", "bind": bind})
        info["plan"] = plan
        r["b1 without approve: approval is asked"] = plan.get("approval_required") is True
        await asyncio.sleep(1.0)
        r["b2 and nothing is created"] = not base.exists() or not any(base.iterdir())
        b = await user({"type": "import_subtree", "path": "out/team", "parent_id": "newparent", "bind": bind, "approve": True})
        info["import_reply"] = b
        ids = {o: n for o, n in zip(TEAM, b.get("ids") or [])} if isinstance(b.get("ids"), list) else dict(b.get("ids") or {})
        info["ids"] = ids
        r["b3 three agents imported under newparent"] = b.get("imported") == 3 and b.get("parent_id") == "newparent"
        r["b4 each has a fresh id (the _<k> suffix)"] = set(ids) == set(TEAM) and all(re.fullmatch(rf"{o}_\d+", n) for o, n in ids.items())
        # the records reach the disk on the loader's debounce
        recs: dict = {}
        end = time.time() + 20
        while time.time() < end and len(recs) < 3:
            recs = {o: rig.host_record(n) for o, n in ids.items() if rig.host_record(n)}
            await asyncio.sleep(0.25)
        info["new_records"] = recs
        r["b5 the three records are persisted"] = len(recs) == 3
        eq = {}
        for o in TEAM:
            want = {**mapped(orig[o], ids), "id": ids.get(o)}
            got = recs.get(o) or {}
            eq[o] = {k: v for k, v in want.items() if got.get(k) != v}
        info["record_diff"] = eq
        r["b6 the records equal the originals (what a blueprint carries)"] = len(recs) == 3 and not any(eq.values())
        lead, mid = recs.get("lead") or {}, recs.get("mid") or {}
        r["b7 the inside grant points at the new ids"] = (lead.get("can_call") or {}).get(ids.get("mid")) == "*" and (mid.get("can_call") or {}).get(ids.get("leaf")) == "*"
        r["b8 the port grant points at files"] = (lead.get("can_call") or {}).get(OUTSIDE) == ["list"]
        nested = [(base / ids.get("lead", "?") / "agent.yaml"), (base / ids.get("lead", "?") / "agents" / ids.get("mid", "?") / "agent.yaml"),
                  (base / ids.get("lead", "?") / "agents" / ids.get("mid", "?") / "agents" / ids.get("leaf", "?") / "agent.yaml")]
        r["b9 they nest lead > mid > leaf under newparent on disk"] = all(p.is_file() for p in nested)

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
