"""E126 import_refusals — what `import_subtree` will not take (#269).

Hand-written blueprint folders under the project, imported under `team_home`:

a) `api_key: sk-planted…` on an agent, `approve: true`: refused `secret_found`; the
   message names `agent a: a api_key key` and `export again` and does NOT hold the key;
   nothing is created;
b) handler `py_ab12.tools` with files `s/bundle.yaml` and `s/tools.py`: refused
   `code_not_allowed`, the message naming the handler, `s/tools.py` and `allow_code: true`;
   nothing is created;
c) a built-in handler with a code file `x/tools.py`: refused the same way; with
   `allow_code: true, approve: true` it imports 1 (the refusal is the gate, not the folder);
d) `approve: "true"` (a string) only previews; `allow_code: "true"` is still refused;
e) a folder with no `blueprint.yaml`: `not_found`, naming the file."""

from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.blueprint import PARENT, agent_ids, importing, world, write_blueprint  # noqa: E402
from _lib.canvas_live import Rig, verdict  # noqa: E402

KEY = "sk-planted0123456789"
SECRET = f"agents:\n  - id: a\n    handler_module: ram_store.tools\n    api_key: {KEY}\n"
CODE = "agents:\n  - id: s\n    handler_module: py_ab12.tools\n"
ONE = "agents:\n  - id: x\n    handler_module: ram_store.tools\n"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        rig.start()
        world(rig)
        before = agent_ids(rig)

        # a) a key in the blueprint
        write_blueprint(rig, "secret", SECRET)
        a = importing(rig, "secret", PARENT, approve=True)
        info["secret"] = a
        msg = str(a.get("error", ""))
        r["a1 a key is refused secret_found"] = a.get("reason") == "secret_found"
        r["a2 it names the agent and the key"] = "agent a: a api_key key" in msg and "export again" in msg
        r["a3 the key value is never echoed"] = KEY not in json.dumps(a)
        r["a4 nothing was created"] = agent_ids(rig) == before

        # b) code that is not a built-in
        write_blueprint(rig, "code", CODE, {"s/bundle.yaml": "name: s\n", "s/tools.py": "def hello():\n    return 1\n"})
        b = importing(rig, "code", PARENT, approve=True)
        info["code"] = b
        msg = str(b.get("error", ""))
        r["b1 code is refused code_not_allowed"] = b.get("reason") == "code_not_allowed"
        r["b2 it names the handler, the file and the flag"] = all(w in msg for w in ("py_ab12.tools", "s/tools.py", "allow_code: true"))
        r["b3 the preview is refused too (no approve)"] = importing(rig, "code", PARENT).get("reason") == "code_not_allowed"
        r["b4 nothing was created"] = agent_ids(rig) == before

        # c) the gate, not the folder: a code file beside a built-in handler
        write_blueprint(rig, "withfile", ONE, {"x/tools.py": "def hello():\n    return 1\n"})
        c0 = importing(rig, "withfile", PARENT, approve=True)
        r["c1 a code file is refused without allow_code"] = c0.get("reason") == "code_not_allowed" and "x/tools.py" in str(c0.get("error"))
        r["c2 still nothing created"] = agent_ids(rig) == before
        c1 = importing(rig, "withfile", PARENT, approve=True, allow_code=True)
        info["allowed"] = c1
        r["c3 allow_code lets the same folder in"] = c1.get("imported") == 1 and "x_1" in agent_ids(rig)

        # d) only the literal true counts
        n = agent_ids(rig)
        d1 = importing(rig, "withfile", PARENT, approve="true", allow_code=True)
        info["approve_string"] = d1
        r["d1 approve as a string only previews"] = d1.get("approval_required") is True and "imported" not in d1 and agent_ids(rig) == n
        d2 = importing(rig, "withfile", PARENT, approve=True, allow_code="true")
        info["allow_code_string"] = d2
        r["d2 allow_code as a string is still refused"] = d2.get("reason") == "code_not_allowed" and agent_ids(rig) == n

        # e) no blueprint
        (rig.proj / "empty").mkdir()
        e = importing(rig, "empty", PARENT, approve=True)
        info["empty"] = e
        r["e1 a folder without blueprint.yaml is not_found"] = e.get("reason") == "not_found" and "blueprint.yaml" in str(e.get("error"))
        e2 = importing(rig, "nowhere", PARENT, approve=True)
        r["e2 a missing folder is not_found"] = e2.get("reason") == "not_found" and "blueprint.yaml" in str(e2.get("error"))
        r["e3 nothing was created"] = agent_ids(rig) == n
    finally:
        rig.stop()
    rig.summary["checks"], rig.summary["info"] = r, info
    failed = [k for k, v in r.items() if not v]
    print(json.dumps(info, indent=1, default=str)[:3000])
    return verdict(*rig.finish(bool(r) and not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
