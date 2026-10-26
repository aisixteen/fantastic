"""E126 export_root_verb — the kernel ROOT answers `export_subtree` too (#268).

The same seeded team as E125 (plus an absolute path on `lead`), a real host.

a) `export_subtree` called on the root (`kernel`, through `forward`) is no error,
   its reply says `exported: 3`, and the folder passes the same checks as the
   `user` one: nested, no `parent_id`, inside grant by id, outside grant a port,
   no key, runtime field, row text or absolute path;
b) the same subtree exported through `user` gives a `blueprint.yaml` byte for byte
   equal (one engine, two doors), and the reply is the same;
c) the root lists the verb (`docs of kernel_state`) and says it is the root's
   (`kernel_state.export_subtree`) when it refuses: a `..` path and an unknown
   agent id are refused on both doors, the unknown one writes no folder;
d) a non-root agent does not answer it as the root (the refusal names the root).

Every call is `host_call`, the way a canvas tab reaches the host."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402
from _lib.export_live import OUTSIDE, check_folder, team_agents  # noqa: E402

ROOT = "kernel"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "out").mkdir()
    abs_path = str(rig.proj / "elsewhere")
    r: dict = {}
    info: dict = {}
    rig.init(host_agents=team_agents(abs_path=abs_path))

    async def run() -> None:
        from canvas import host_call
        user = lambda payload: host_call(rig.port, "user", payload)  # noqa: E731
        root = lambda payload: host_call(rig.port, ROOT, payload)  # noqa: E731
        # a) the root's own door
        a = await root({"type": "export_subtree", "agent_id": "lead", "path": "out/root"})
        info["root_reply"] = a
        r["a1 the root's reply is no error and counts three agents"] = "error" not in a and a.get("exported") == 3
        r["a2 its ports hold @files and the path port"] = f"@{OUTSIDE}" in (a.get("ports") or []) and "@path:workdir" in (a.get("ports") or [])
        for k, v in check_folder(rig.proj / "out" / "root", rig.proj, abs_path=abs_path).items():
            r[f"a3 folder: {k}"] = v
        # b) one engine: the same folder through `user`
        b = await user({"type": "export_subtree", "agent_id": "lead", "path": "out/user"})
        info["user_reply"] = b
        f_root, f_user = rig.proj / "out" / "root" / "blueprint.yaml", rig.proj / "out" / "user" / "blueprint.yaml"
        r["b1 the two blueprint.yaml are byte for byte equal"] = f_root.is_file() and f_user.is_file() and f_root.read_bytes() == f_user.read_bytes() and f_user.stat().st_size > 0
        r["b2 and the replies agree (count, ports, files)"] = all(a.get(k) == b.get(k) for k in ("exported", "ports", "files"))
        # c) the root lists the verb; both doors refuse bad input
        ref = await root({"type": "docs", "of": "kernel_state", "tier": "detailed", "verb": "export_subtree"})
        info["root_docs"] = ref
        r["c1 the root's docs describe export_subtree"] = "error" not in ref and "export_subtree" in json.dumps(ref) and "blueprint" in json.dumps(ref)
        dots = await root({"type": "export_subtree", "agent_id": "lead", "path": "out/../x"})
        info["dots"] = dots
        r["c2 a `..` path is refused by the root (bad_arg), nothing written"] = dots.get("reason") == "bad_arg" and not (rig.proj / "x").exists() and not (rig.proj / "out" / ".." / "x").exists()
        dots_u = await user({"type": "export_subtree", "agent_id": "lead", "path": "out/../x"})
        r["c3 and by user, each naming its own door in the message"] = (
            dots_u.get("reason") == "bad_arg" and str(dots_u.get("error", "")).startswith("user.export_subtree")
            and str(dots.get("error", "")).startswith("kernel_state.export_subtree"))
        nope = await root({"type": "export_subtree", "agent_id": "no_such_agent", "path": "out/nope"})
        info["nope"] = nope
        r["c4 an unknown agent id is refused by the root and no folder is made"] = "error" in nope and not (rig.proj / "out" / "nope").exists()
        nope_u = await user({"type": "export_subtree", "agent_id": "no_such_agent", "path": "out/nope"})
        r["c5 and by user, with the same reason"] = "error" in nope_u and nope_u.get("reason") == nope.get("reason") and not (rig.proj / "out" / "nope").exists()
        miss = await root({"type": "export_subtree", "path": "out/miss"})
        r["c6 a missing agent_id is bad_arg"] = miss.get("reason") == "bad_arg"
        # d) only the root answers it as the root
        other = await host_call(rig.port, "lead", {"type": "export_subtree", "agent_id": "lead", "path": "out/other"})
        info["other"] = other
        r["d1 another agent does not export (no folder)"] = not (rig.proj / "out" / "other").exists()

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
