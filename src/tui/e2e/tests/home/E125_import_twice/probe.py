"""E125 import_twice — a blueprint imported twice (#269).

a) a hand-written blueprint `bp/` (squad > lead, scout; `lead` reaches `@memory`) and a
   second store `memory2` to bind;
b) `import_subtree` WITHOUT `approve` under `team_home`, `@memory` bound to `memory2`:
   `approval_required`, 3 agents, fresh `*_1` ids, `reaches` and `ports` named, and the
   agent count UNCHANGED (a preview creates nothing);
c) the same call with `approve: true`: 3 imported, `*_1` ids, the parents right on disk,
   the grants re-wired to the new ids, and `memory2` is `callable_by` the new `lead_1`;
d) imported again under the same parent: `*_2` ids, copy 1 untouched; a field edited on
   copy 1 leaves copy 2 as it was (two independent copies);
e) with `bind` omitted the call is refused `unbound_ports`, naming `@memory (memory)` and
   `pass bind`; nothing is created."""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.blueprint import PARENT, STORE, agent_ids, importing, record, send, world, write_team  # noqa: E402
from _lib.canvas_live import Rig, verdict  # noqa: E402

BIND = {"@memory": STORE}


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        rig.start()
        world(rig)
        write_team(rig, "bp")
        before = agent_ids(rig)
        r["a0 the parent and the store to bind exist"] = PARENT in before and STORE in before

        # b) the preview
        pre = importing(rig, "bp", PARENT, BIND)
        info["preview"] = pre
        r["b1 the preview asks for approval"] = pre.get("approval_required") is True
        r["b2 it plans three agents with fresh _1 ids"] = pre.get("agents") == 3 and pre.get("new_ids") == {"squad": "squad_1", "lead": "lead_1", "scout": "scout_1"}
        reaches = {x.get("id"): x for x in pre.get("reaches", [])}
        r["b3 it names what lead reaches and the port"] = (
            sorted(reaches.get("lead_1", {}).get("can_call", [])) == [STORE, "scout_1"] and pre.get("ports") == BIND)
        r["b4 it created nothing"] = agent_ids(rig) == before

        # c) approved
        done = importing(rig, "bp", PARENT, BIND, approve=True)
        info["imported"] = done
        r["c1 three agents imported with _1 ids"] = done.get("imported") == 3 and set(done.get("ids", {}).values()) == {"squad_1", "lead_1", "scout_1"}
        r["c2 the home holds them"] = agent_ids(rig) == sorted([*before, "squad_1", "lead_1", "scout_1"])
        parents = {i: record(rig, i).get("parent_id") for i in ("squad_1", "lead_1", "scout_1")}
        info["parents_1"] = parents
        r["c3 parents are right on disk"] = parents == {"squad_1": PARENT, "lead_1": "squad_1", "scout_1": "squad_1"}
        lead = record(rig, "lead_1")
        r["c4 lead's grants and store point at the new ids and the bound store"] = (
            set(lead.get("can_call", {})) >= {"scout_1", STORE} and lead.get("memory_store_id") == STORE and "@memory" not in json.dumps(lead))
        r["c5 scout is callable by the new lead"] = "lead_1" in record(rig, "scout_1").get("callable_by", {})
        r["c6 the bound store gained the new lead"] = "lead_1" in send(rig, STORE, {"type": "reflect"}).get("callable_by", {})

        # d) twice: two independent copies
        again = importing(rig, "bp", PARENT, BIND, approve=True)
        info["imported_2"] = again
        r["d1 the second import has _2 ids"] = again.get("imported") == 3 and set(again.get("ids", {}).values()) == {"squad_2", "lead_2", "scout_2"}
        r["d2 the first copy is still there"] = {"squad_1", "lead_1", "scout_1"} <= set(agent_ids(rig))
        r["d3 the second copy is under the same parent"] = record(rig, "squad_2").get("parent_id") == PARENT
        r["d4 the second copy has its own grants"] = (
            set(record(rig, "lead_2").get("can_call", {})) >= {"scout_2", STORE} and "lead_2" in send(rig, STORE, {"type": "reflect"}).get("callable_by", {}))
        edit = send(rig, "kernel_state", {"type": "update_agent", "id": "lead_1", "note": "edited"})
        info["edit"] = edit
        end = time.time() + 10
        while time.time() < end and record(rig, "lead_1", 1).get("note") != "edited":
            time.sleep(0.2)
        r["d5 editing copy 1 changes copy 1"] = record(rig, "lead_1").get("note") == "edited"
        r["d6 copy 2 is unchanged"] = record(rig, "lead_2").get("note") == "first"

        # e) the port left unbound
        n = agent_ids(rig)
        refused = importing(rig, "bp", PARENT, {}, approve=True)
        info["unbound"] = refused
        msg = str(refused.get("error", ""))
        r["e1 an unbound port is refused"] = refused.get("reason") == "unbound_ports" and "@memory (memory)" in msg and "pass bind" in msg
        r["e2 nothing was created"] = agent_ids(rig) == n
    finally:
        rig.stop()
    rig.summary["checks"], rig.summary["info"] = r, info
    failed = [k for k, v in r.items() if not v]
    print(json.dumps(info, indent=1, default=str)[:3000])
    return verdict(*rig.finish(bool(r) and not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
