"""E125 cabinets (#132) on the REAL host — a master opens 3 cabinets, the workers build and
talk, their status shows live on the main canvas, the master monitors and concludes:

  C1 OPEN       three `open_cabinet` calls in ONE turn: 3 cabinets, 3 canvases (children of the
                main canvas), 3 workers; each worker is a copy of the master's backend (no new
                backend), knows exactly its 2 peers, may only message them and the master, holds
                `create`/`place` on its own canvas and nothing on the root; the canvas it builds
                on holds `create_agent` on the root and nothing else (never the reach of an AI).
  C2 EQUIPMENT  each worker `create`s an exec agent on ITS canvas; once a grant names it (the
                person's step: no grant is automatic) the worker runs it and reads its output;
                another cabinet's worker is refused on it.
  C3 TALK       a worker sends a peer a message; the peer sees it and reports it up.
  C4 STATUS UP  each worker reports `working` then `done`; the status cells placed on the main canvas
                show both, in order, with no reload and no polling (the cell `watch`es).
  C5 MONITOR    the master re-tasks one worker (it gets the new task) and stops another mid-turn
                (idle, no further model turn); the cabinets reply carries the same lines.
  C6 CONCLUDE   `conclude` closes all 3: no cabinet, worker, canvas or equipment is left in the
                kernel or on disk, no grant names a closed id, the manager and master remain.
  C7 CLEAN      no page error, no panic in the host log."""

from __future__ import annotations

import asyncio
import json
import re
import sys
import threading
import time
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e"))
sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e" / "runners"))
from _lib.canvas_live import FOLDER, Rig, private_write, saved_dir, spot, verdict  # noqa: E402
from _lib.dev_surface import frame_eval  # noqa: E402
from canvas import host_call  # noqa: E402
from mocks import DONE  # noqa: E402
from mocks.vllm import VllmMock  # noqa: E402

NAMES = ("alpha", "beta", "gamma")
MANAGER = "cabinet_manager"
ROOT = "kernel_state"
MAIN = "canvas_root"
ROOT_VERBS = ["create_agent", "update_agent", "delete_agent"]
WORKER_AT = re.compile(r"worker AI of cabinet `(cabinet_\w+)`")


def xml(v) -> str:
    """A value in the call grammar: maps are tags, lists are `<item>`s."""
    if isinstance(v, dict):
        return "".join(f"<{k}>{xml(w)}</{k}>" for k, w in v.items())
    if isinstance(v, list):
        return "".join(f"<item>{xml(w)}</item>" for w in v)
    return str(v)


def call(target: str, verb: str, **fields) -> str:
    return f'<call name="send">\n<target_id>{target}</target_id>\n<type>{verb}</type>\n' + "".join(f"<{k}>{xml(v)}</{k}>\n" for k, v in fields.items()) + "</call>\n"


def sysblock(req: dict) -> str:
    return " ".join(m.get("content", "") if isinstance(m.get("content"), str) else json.dumps(m.get("content"))
                    for m in req.get("messages", []) if m.get("role") == "system")


def newest(req: dict) -> str:
    c = (req.get("messages") or [{}])[-1].get("content", "")
    return c if isinstance(c, str) else json.dumps(c)


def untag(text: str) -> str:
    return text.replace("\\u003c", "<").replace("\\u003e", ">")


class Script:
    """The scripted AIs on ONE mock: the master and a worker per cabinet. Which AI a turn belongs to is
    read from its system block (a worker's carries `worker AI of cabinet ...`); each AI has its own
    phase, advanced by the markers the test sends it. A turn that is no marker is the RESULT of the
    batch the AI just made: it ends the chain (`done`)."""

    def __init__(self, view: str):
        self.view = view
        self.lock = threading.Lock()
        self.phase: dict[str, int] = {}
        self.seen: list[tuple[str, str]] = []  # (who, newest message) in order
        self.results: dict[str, list[str]] = {}  # who -> the result turns, in order
        self.hold = threading.Event()  # beta's hung turn waits on it
        self.job: dict[str, str] = {}

    def who(self, req: dict) -> str:
        m = WORKER_AT.search(sysblock(req))
        return m.group(1) if m else "master"

    def __call__(self, req: dict) -> str | None:
        who, last = self.who(req), untag(newest(req))
        if last.lstrip().startswith(("[no call was dispatched]", "[no answer]")):
            return DONE
        with self.lock:
            self.seen.append((who, last))
            out = self.master(last) if who == "master" else self.worker(who, last)
        if out == "HANG":
            self.hold.wait(60)
            return DONE
        return out

    # ── the master: one batch per marker ────────────────────────────────────
    def master(self, last: str) -> str:
        if "E125-open" in last:
            return "".join(call(MANAGER, "open_cabinet", task=f"E125 job {n}", name=n) for n in NAMES)
        if "E125-place" in last:
            return "".join(call(self.view, "place", backend_id=f"cabinet_{n}", x=80 + 330 * i, y=420, width=320, height=240) for i, n in enumerate(NAMES))
        if "E125-go" in last:
            return "".join(call(f"cabinet_{n}_worker", "send", text=f"E125-task {n}") for n in NAMES)
        if "E125-cabinets" in last:
            return call(MANAGER, "cabinets")
        if "E125-retask" in last:
            return call("cabinet_alpha_worker", "send", text="E125-new alpha task: count to three") + call("cabinet_beta_worker", "stop")
        if "E125-conclude" in last:
            return call(MANAGER, "conclude")
        self.results.setdefault("master", []).append(last)
        return DONE

    # ── a worker: the markers the test sends it ─────────────────────────────
    def worker(self, who: str, last: str) -> str:
        n = who.split("_", 1)[1]
        canvas, run = f"canvas_{who}", f"{n}_run"
        if "E125-task" in last:  # report, then build equipment on ITS canvas
            return (call(who, "status", line="working", progress=30)
                    + call(canvas, "create", record={"handler_module": "exec_bridge.tools", "id": run, "allowed": ["^echo E125-.*$"], "cwd": ".", "inherit_env": ["*"], "ingress_rule": "allow_all_ingress"}))
        if "E125-talk" in last:  # alpha pings beta
            return call("cabinet_beta_worker", "send", text="E125 PING from alpha")
        if "E125 PING from alpha" in last:  # the peer reports what it heard
            return call(who, "status", line="got ping from alpha", progress=50)
        if "E125-run" in last:  # run the equipment; beta also reaches for alpha's
            out = call(run, "start", command="echo", args=[f"E125-{n}"]) + call(canvas, "place", backend_id=run)
            return out + (call("alpha_run", "start", command="echo", args=["E125-FROM-BETA"]) if n == "beta" else "")
        if "E125-tail" in last:
            return call(run, "tail", job_id=self.job.get(who, "0"), lines=5)
        if "E125-finish" in last:
            return call(who, "status", line="done", progress=100, result={"cabinet": n, "answer": f"E125-result-{n}"})
        if "E125-new alpha task" in last:  # the re-task shows in the cell
            return call(who, "status", line="retasked: count to three", progress=10)
        if "E125-hang" in last:
            return "HANG"
        m = re.search(r"<job_id>(\d+)</job_id>", last)
        if m:
            self.job[who] = m.group(1)
        self.results.setdefault(who, []).append(last)
        return DONE


def main() -> bool:
    mock = VllmMock(model="mock-x").start()
    rig = Rig(__file__, live=False)
    rig.build("vllm")
    rig.make_home(None)
    private_write(saved_dir(rig.home) / "mock.yaml", yaml.safe_dump(
        {"handler_module": "vllm_backend.tools", "endpoint": mock.url, "model": "mock-x", "kv_overflow": "evict"}, sort_keys=False))
    private_write(saved_dir(rig.home) / ".preferred", "mock")
    r: dict = {}
    info: dict = {}
    script = Script("")
    mock.responder = script
    proj = rig.proj / ".fantastic"

    def rec(i: str) -> dict:
        for _ in range(5):
            try:
                return rig.host_record(i) or {}
            except (OSError, yaml.YAMLError):  # a record deleted or half written while it is read
                time.sleep(0.1)
        return {}

    def until(cond, secs: float = 15.0) -> bool:
        end = time.time() + secs
        while time.time() < end:
            if cond():
                return True
            time.sleep(0.2)
        return bool(cond())

    def worker_turns(cab: str) -> list[str]:
        return [last for who, last in script.seen if who == cab]

    def walk(pattern: str) -> list[Path]:
        """The matching files under the project; a directory deleted while it is walked is walked again."""
        for _ in range(20):
            try:
                return list(proj.rglob(pattern))
            except FileNotFoundError:
                time.sleep(0.1)
        return []

    def on_disk(pattern: str) -> list[str]:
        return sorted({str(p.parent.relative_to(proj)) for p in walk(f"{pattern}/agent.yaml")})

    async def tree_ids() -> set[str]:
        out: set[str] = set()

        def walk(n: dict) -> None:
            out.add(n["id"])
            for c in n.get("children") or []:
                walk(c)
        walk((await host_call(rig.port, ROOT, {"type": "reflect"}))["tree"])
        return out

    async def human(target: str, payload: dict) -> dict:
        got = await host_call(rig.port, target, payload)
        if payload.get("type") == "send":
            info.setdefault("human_sends", []).append([target, payload.get("text"), str(got)[:140]])
        return got

    async def master_says(master: str, text: str) -> bool:
        """Send the master a message and wait for the result turn of the batch it makes (its end)."""
        n = len(script.results.get("master", []))
        await human(master, {"type": "send", "text": text})
        got = until(lambda: len(script.results.get("master", [])) > n, 30)
        # ...and until the turn is over: a message sent meanwhile queues behind it as a note
        for _ in range(60):
            st = await human(master, {"type": "status"})
            if st.get("generating") is False and not st.get("mine_pending"):
                break
            await asyncio.sleep(0.25)
        return got

    async def grant_equipment(n: str) -> None:
        """The person's step: the worker may run (and read) its own equipment."""
        w, run = f"cabinet_{n}_worker", f"{n}_run"
        cc = dict(rec(w)["can_call"])
        cc[run] = ["start", "tail"]
        cb = dict(rec(run)["callable_by"])
        cb[w] = ["start", "tail"]
        for target, key, val in ((w, "can_call", cc), (run, "callable_by", cb)):
            got = await human(ROOT, {"type": "update_agent", "id": target, key: val})
            assert not got.get("error"), got

    try:
        rig.init(host_agents=[(FOLDER, {"id": MANAGER, "handler_module": "cabinet.tools", "cabinet_role": "manager", "canvas_id": MAIN,
                                         "can_call": {ROOT: ROOT_VERBS}})])

        async def run() -> None:
            async with rig.page(1600, 1000) as pg:
                # the operator's half of the set-up (readme "Setting up the manager"): the root names the manager
                root = await human(ROOT, {"type": "reflect"})
                cb = dict(root["callable_by"])
                cb[MANAGER] = ROOT_VERBS
                got = await human(ROOT, {"type": "update_agent", "id": ROOT, "callable_by": cb})
                assert not got.get("error"), got
                script.view = f"{rig.page_guid()}|view_root"
                # the MASTER: an AI cell, as a person opens one
                await spot(pg, 300, 200, "E125-hello")
                await pg.until("document.querySelectorAll('.chat-view').length === 1", 15)
                until(lambda: any(x.get("backend_id") for x in rig.cells()), 15)
                master = next(x["backend_id"] for x in rig.cells() if x.get("backend_id"))
                info["master"] = master
                # the AI is made and settled before it works (its cell, its grants, its first turn)
                until(lambda: script.results.get("master"), 20)
                await asyncio.sleep(2)
                await master_says(master, "E125-open the cabinets")
                workers = [f"cabinet_{n}_worker" for n in NAMES]
                cabs = [f"cabinet_{n}" for n in NAMES]
                canvases = [f"canvas_{c}" for c in cabs]

                # ── C1 ─────────────────────────────────────────────────────────
                until(lambda: all(rec(w).get("cabinet_peers") and len(rec(w)["cabinet_peers"]) == 2 for w in workers), 30)
                until(lambda: all(rec(c).get("cabinet_role") == "cabinet" for c in cabs), 5)
                roles = sorted(i for i in on_disk("cabinet_*") if rec(Path(i).name).get("cabinet_role") == "cabinet")
                info["C1_cabinets"] = roles
                r["C1 exactly 3 cabinets, 3 workers, 3 canvases under the main canvas"] = (
                    [Path(i).name for i in roles] == cabs
                    and all(rec(w).get("parent_id") == c for w, c in zip(workers, cabs))
                    and all(rec(cv).get("parent_id") == MAIN and rec(cv).get("handler_module") == "canvas.tools" for cv in canvases)
                    and all(rec(c).get("canvas") == cv for c, cv in zip(cabs, canvases)))
                mrec = rec(master)
                same = all(rec(w).get(k) == mrec.get(k) for w in workers for k in ("endpoint", "model", "handler_module", "memory_store_id"))
                backends = sorted(Path(i).name for i in on_disk("*") if rec(Path(i).name).get("handler_module") == "vllm_backend.tools")
                r["C1 workers are copies of the master's backend: same endpoint, model, store; no new backend"] = (
                    same and not any(rec(w).get("new_backend") for w in workers)
                    and backends == sorted([master, *workers]) and all(rec(w).get("saved") == mrec.get("saved") for w in workers))
                peers_ok = True
                for w, c in zip(workers, cabs):
                    want = sorted([(o, ow) for o, ow in zip(cabs, workers) if o != c])
                    got_peers = sorted((p["cabinet"], p["worker"]) for p in rec(w).get("cabinet_peers") or [])
                    peers_ok &= got_peers == want
                r["C1 each worker knows exactly the other 2 cabinets and workers"] = peers_ok
                root_now = await human(ROOT, {"type": "reflect"})
                grants_ok = True
                for w, c, cv in zip(workers, cabs, canvases):
                    cc = rec(w).get("can_call") or {}
                    others = [o for o in workers if o != w]
                    grants_ok &= all(cc.get(o) == ["send", "reflect"] for o in others) and cc.get(master) == ["send", "reflect"]
                    grants_ok &= cc.get(cv) == ["create", "place"] and cc.get("$parent") == ["status"]
                    grants_ok &= MAIN not in cc and ROOT not in cc and MANAGER not in cc
                    # the canvas it builds on: `create_agent` on the root and nothing else
                    cvc = rec(cv).get("can_call") or {}
                    grants_ok &= set(cvc) == {"$self", ROOT} and cvc.get(ROOT) == ["create_agent"]
                    grants_ok &= root_now["callable_by"].get(cv) == ["create_agent"]
                info["C1_root_callable_by"] = {k: v for k, v in root_now["callable_by"].items() if "cabinet" in k}
                info["C1_all_can_call"] = {w: rec(w).get("can_call") for w in workers}
                info["C1_all_canvas_can_call"] = {cv: rec(cv).get("can_call") for cv in canvases}
                info["C1_canvas_can_call"] = rec(canvases[0]).get("can_call")
                r["C1 grants: workers message only; create+place on their own canvas; the canvas holds create_agent only"] = grants_ok
                await asyncio.sleep(2)
                urec = rec("user")
                info["C1_user_reaches"] = {w: [w in (urec.get("can_call") or {}), "user" in (rec(w).get("callable_by") or {}), "kernel" in (rec(w).get("can_call") or {})] for w in workers}
                # the master: places the three status cells on the main canvas
                await master_says(master, "E125-place the cells")
                ok = await pg.until("document.querySelectorAll('.agent-frame iframe').length >= 3", 20)
                until(lambda: len(re.findall(r"<placed>", " ".join(script.results.get("master", [])))) >= 1, 15)
                placed = re.findall(r"<placed>(\w+)</placed>\s*<shows>(\w+)</shows>", " ".join(script.results.get("master", [])))
                cells = {c: next((p for p, shows in placed if shows == c), None) for c in cabs}
                info["cell_records"] = [x for x in rig.cells() if x.get("id") in cells.values()][:1]
                info["cells"] = cells
                await pg.until("true", 0.1)
                # the status cells are SANDBOXED panels (their own process): a probe inside each frame keeps
                # every line the cell shows, from now on, in the frame's own memory
                watch_js = ("(()=>{if (typeof ID==='undefined' || !document.getElementById('line')) return null; const el=document.getElementById('line');"
                            " if (!window.__h) { window.__h=[el.textContent]; new MutationObserver(()=>window.__h.push(el.textContent)).observe(el,{childList:true,characterData:true,subtree:true}); }"
                            " return ID})()")
                seen_cells: set = set()
                for _ in range(100):
                    seen_cells = {x for x in await frame_eval(pg, watch_js) if x}
                    if seen_cells >= set(cabs):
                        break
                    await asyncio.sleep(0.2)
                info["C4_watched"] = sorted(seen_cells)
                r["C1 the master placed three live status cells on the main canvas"] = ok and all(cells.values()) and seen_cells >= set(cabs)

                async def lines() -> dict:
                    out = {}
                    for v in await frame_eval(pg, "({id: typeof ID==='undefined'?null:ID, h: window.__h||null})"):
                        if v and v.get("id"):
                            out[v["id"]] = v["h"] or []
                    return out
                await pg.js("window.__e125_marker = 'still here'; window.__e125_navs = performance.getEntriesByType('navigation').length")
                views0 = sum(1 for ln in rig.log.read_text(errors="replace").splitlines() if re.search(r"\] cabinet_\w+ call view$", ln))

                # ── C2 equipment ───────────────────────────────────────────────
                await master_says(master, "E125-go")
                runs = [f"{n}_run" for n in NAMES]
                made = until(lambda: all(rec(x) for x in runs), 30)
                info["C2_equipment_parents"] = {x: rec(x).get("parent_id") for x in runs}
                r["C2 each worker built exec equipment on ITS OWN cabinet canvas"] = made and all(rec(x).get("parent_id") == cv for x, cv in zip(runs, canvases))
                # not a grant of the worker's own making: the equipment names no other cabinet
                def reach_of(w: str) -> list:
                    return [w in (rec("user").get("can_call") or {}), "user" in (rec(w).get("callable_by") or {}), (rec("user").get("can_call") or {}).get(w), (rec(w).get("callable_by") or {}).get("user")]
                info["reach_before_grant"] = {w: reach_of(w) for w in workers}
                for n in NAMES:
                    await grant_equipment(n)
                    info.setdefault("reach_after_grant", {})[n] = {w: reach_of(w) for w in workers}
                for w in workers:
                    await human(w, {"type": "send", "text": "E125-run"})
                until(lambda: all(any("job_id" in t or "may not call" in t for t in script.results.get(c, [])) for c in cabs), 30)
                for w in workers:
                    await human(w, {"type": "send", "text": "E125-tail"})
                until(lambda: all(any(f"E125-{n}" in t for t in script.results.get(f"cabinet_{n}", [])) for n in NAMES), 30)
                res_a = " ".join(script.results.get("cabinet_alpha", []))
                res_b = " ".join(script.results.get("cabinet_beta", []))
                res_g = " ".join(script.results.get("cabinet_gamma", []))
                info["C2_beta_results"] = res_b[-700:]
                info["C2_alpha_place_on_its_canvas"] = res_a[res_a.find("canvas_cabinet_alpha place"):][:300]
                r["C2 a worker runs its own equipment and reads its output (echo marker per cabinet)"] = (
                    "E125-alpha" in res_a and "E125-beta" in res_b and "E125-gamma" in res_g)
                r["C2 another cabinet's equipment is refused (beta on alpha_run), and ran nothing"] = (
                    "may not call 'start' on alpha_run" in res_b and "E125-FROM-BETA" not in res_a)

                # ── C3 talk ────────────────────────────────────────────────────
                await human("cabinet_alpha_worker", {"type": "send", "text": "E125-talk"})
                heard = until(lambda: any("E125 PING from alpha" in t for t in worker_turns("cabinet_beta")), 20)
                said = until(lambda: "got ping" in ((rec("cabinet_beta").get("status") or {}).get("line") or ""), 20)
                r["C3 the peer received the ping text"] = heard
                r["C3 the peer reported it up to its cabinet"] = said

                # ── C4 status up ───────────────────────────────────────────────
                for w in workers:
                    await human(w, {"type": "send", "text": "E125-finish"})
                until(lambda: all((rec(c).get("status") or {}).get("line") == "done" for c in cabs), 30)
                await asyncio.sleep(1.5)
                hist = await lines()
                info["C4_history"] = hist
                info["C4_reach"] = {c: {"callable_by": rec(c).get("callable_by"), "leg": (rec("canvas_ws").get("can_call") or {}).get(c), "user": (rec("user").get("can_call") or {}).get(c)} for c in cabs}
                order = True
                for c in cabs:
                    h = hist.get(c) or []
                    order &= "working" in h and "done" in h and h.index("working") < h.index("done")
                r["C4 each status cell shows 'working' and then 'done' as they are reported"] = bool(hist) and order
                alive = await pg.js("[window.__e125_marker, performance.getEntriesByType('navigation').length === window.__e125_navs]")
                r["C4 ...with no reload of the page"] = alive == ["still here", True]
                views1 = sum(1 for ln in rig.log.read_text(errors="replace").splitlines() if re.search(r"\] cabinet_\w+ call view$", ln))
                statuses = sum(1 for ln in rig.log.read_text(errors="replace").splitlines() if re.search(r"\] cabinet_\w+ call status$", ln))
                info["C4_views_status"] = [views1 - views0, statuses]
                r["C4 ...and no polling: the cells re-read once per update, not on a timer"] = 0 < views1 - views0 <= statuses + 3

                # ── C5 monitor ─────────────────────────────────────────────────
                # a send answers when the turn ends: the hung turn is sent and left running
                hang = asyncio.ensure_future(human("cabinet_beta_worker", {"type": "send", "text": "E125-hang"}))
                hang.add_done_callback(lambda f: f.cancelled() or f.exception())
                hung = False
                for _ in range(60):  # (async: the sent turn runs on this loop)
                    if any("E125-hang" in t for t in worker_turns("cabinet_beta")):
                        hung = True
                        break
                    await asyncio.sleep(0.25)
                await master_says(master, "E125-cabinets list")
                await master_says(master, "E125-retask")
                got_new = until(lambda: any("E125-new alpha task" in t for t in worker_turns("cabinet_alpha")), 20)
                r["C5 the re-tasked worker got the new task text"] = got_new
                live_new = False
                for _ in range(60):
                    h = (await lines()).get("cabinet_alpha") or []
                    if any("retasked" in x for x in h):
                        live_new = True
                        break
                    await asyncio.sleep(0.25)
                r["C5 ...and its cell shows the new line live"] = live_new
                beta_state = await human("cabinet_beta_worker", {"type": "status"})
                info["C5_beta_state"] = beta_state
                n_before = len(worker_turns("cabinet_beta"))
                script.hold.set()
                await asyncio.sleep(2.5)
                idle = beta_state.get("generating") is False and beta_state.get("current") is None
                info["C5_stop"] = [hung, idle, n_before, len(worker_turns("cabinet_beta"))]
                r["C5 a stopped worker is idle and takes no further model turn"] = hung and idle and len(worker_turns("cabinet_beta")) == n_before
                shown = " ".join(script.results.get("master", []))
                info["C5_master_cabinets"] = shown[-900:]
                r["C5 the master's cabinets reply lists the same lines and canvases"] = (
                    all(c in shown for c in cabs) and all(cv in shown for cv in canvases) and "E125-result-alpha" in shown)

                # ── C6 conclude ────────────────────────────────────────────────
                await master_says(master, "E125-conclude")
                dead = [*cabs, *workers, *canvases, *runs, *(f"{c}_loader" for c in canvases)]
                gone = until(lambda: not [p for d in dead for p in on_disk(d)], 30)
                await asyncio.sleep(1.0)
                concl = script.results.get("master", [""])[-1]
                info["C6_reply"] = concl[-700:]
                r["C6 the conclusion lists the 3 cabinets it closed"] = bool(re.search(r"<concluded>(\s*<item>cabinet_\w+</item>){3}\s*</concluded>", concl)) and all(d in concl for d in [*cabs, *workers, *canvases, *runs])
                left_ids = await tree_ids()
                info["C6_left_in_kernel"] = sorted(i for i in dead if i in left_ids)
                info["C6_left_on_disk"] = [p for d in dead for p in on_disk(d)]
                r["C6 nothing of the cabinets is left in the kernel"] = gone and not info["C6_left_in_kernel"]
                r["C6 nothing of the cabinets is left on disk (canvases, cabinets, workers, equipment)"] = not info["C6_left_on_disk"]
                r["C6 the manager, the master and the main canvas remain"] = MANAGER in left_ids and master in left_ids and MAIN in left_ids
                root_end = await human(ROOT, {"type": "reflect"})
                held = {i: [rec(i).get("can_call"), rec(i).get("callable_by")] for i in (MANAGER, master, MAIN)}
                held[ROOT] = [root_end.get("can_call"), root_end.get("callable_by")]
                holders = [i for i, g in held.items() if any(re.search(rf'"{d}"', json.dumps(g)) for d in dead)]
                info["C6_holders_naming_closed_ids"] = holders
                stale = []
                for p in walk("agent.yaml"):
                    try:
                        text = p.read_text()
                    except OSError:
                        continue
                    if any(re.search(rf"(^|[^\w]){d}([^\w]|$)", text) for d in dead) and "cabinet_peers" not in text:
                        stale.append(str(p.relative_to(proj)))
                info["C6_records_naming_closed_ids"] = stale[:8]
                r["C6 no grant of a surviving record names a closed cabinet id"] = not holders and not stale
                after = await human("cabinet_alpha", {"type": "view"})
                r["C6 a closed cabinet answers nothing (no watch or call survives)"] = bool(after.get("error"))
                left_cells = [x["id"] for x in rig.cells() if x.get("backend_id") in cabs]
                gone_frames = await pg.until("document.querySelectorAll('.agent-frame iframe').length === 0", 15)
                info["C6_cells_left"] = left_cells
                r["C6 the three status cells are gone from the main canvas"] = not left_cells and bool(gone_frames)

                # ── C7 ─────────────────────────────────────────────────────────
                bad = [x for x in pg.logs if "WebGL" not in str(x)]
                info["C7_page_logs"] = bad[:6]
                panics = [ln for ln in rig.log.read_text(errors="replace").splitlines() if "panicked" in ln]
                info["C7_panics"] = panics[:3]
                r["C7 no page error and no host panic"] = not bad and not panics

        try:
            asyncio.run(run())
        except Exception as e:  # a probe bug or a product fault shows as a failed check, with the traceback
            import traceback
            traceback.print_exc()
            r[f"the run reached its end ({type(e).__name__}: {str(e)[:120]})"] = False
    finally:
        script.hold.set()
        mock.stop()
        rig.stop()
    info["seen"] = [[w, t.strip()[-110:]] for w, t in script.seen]
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:4000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
