"""kernel/folder_bundles — a folder under `.fantastic/bundles/` as an agent kind. NO MODEL.

Folder bundles (`bundle.yaml`, `base: py_agent`, a stdlib program importing the
reference helper `fantastic_agent.py`) on the PACKAGED host of `RUNTIME`
(python | rust), every call through `web_rest`:

  1. COLD LOAD: folders written BEFORE boot are read by the root loader (rust
     through the fs clamp); a seeded instance record resolves and answers;
  2. CATALOG + DOCS: `reflect bundles=all` lists the folder; `docs` reads bundle.yaml;
  3. CREATE: an instance of the folder answers its verbs; reflect names its base
     and a running program; delete kills the program; py_agent itself is a base;
  4. HOT LOAD: a folder added after boot loads on `reload_bundle`; an edited
     program + reload restarts the live instance; a failed reload keeps the
     previous load; reload of no folder is not_found;
  5. NOT FATAL: an unparsable bundle.yaml and one without `sentence` are listed
     with their error; an instance of a refused folder answers `unhandled`;
  6. RESERVED VERBS: a bundle declaring `reflect` is refused; a bundle declaring
     the kernel verb `ps` loads but `ps` on its instance is the kernel's answer;
  7. every program is gone after the kernel stops.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

import yaml
from helpers import oslayer
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
HELPER = (REPO / "src/lib/python/bundled_agents/io/py_agent/src/py_agent"
          / "fantastic_agent.py")
PY = sys.executable  # absolute: the program gets no PATH unless the record says so

ECHO_DOC = "args: text:str (req). Returns {echo, pid}."
NOTES = "E117 notes: the cold bundle's own manual line."
CALL_S = 30


def program(version: str, extra: str = "") -> str:
    return (
        "import os\n"
        "import fantastic_agent as fa\n\n"
        "def echo(p):\n"
        f"    return {{'echo': {version!r} + ':' + str(p.get('text')), 'pid': os.getpid()}}\n\n"
        "def ps(p):\n"
        "    return {'from_program': True}\n\n"
        f"fa.serve({{'echo': echo{extra}}})\n"
    )


def bundle(root: Path, name: str, *, version: str = "v1", verbs: dict | None = None,
           extra: str = "", spec: dict | None = None, raw: str | None = None) -> Path:
    """Write `<root>/.fantastic/bundles/<name>/` — `raw` replaces bundle.yaml verbatim."""
    folder = root / ".fantastic" / "bundles" / name
    folder.mkdir(parents=True, exist_ok=True)
    shutil.copy(HELPER, folder / "fantastic_agent.py")
    (folder / "agent.py").write_text(program(version, extra))
    if raw is not None:
        (folder / "bundle.yaml").write_text(raw)
        return folder
    doc = {
        "base": "py_agent",
        "sentence": f"The {name} test agent.",
        "command": [PY, "agent.py"],
        "verbs": verbs or {"echo": ECHO_DOC},
        **(spec or {}),
    }
    (folder / "bundle.yaml").write_text(yaml.safe_dump(doc, sort_keys=False))
    return folder


def alive(pid) -> bool:
    return isinstance(pid, int) and oslayer.pid_alive(pid)


def gone(pid, within: float = 10.0) -> bool:
    """Has `pid` exited — polled, bounded."""
    end = time.monotonic() + within
    while time.monotonic() < end:
        if not alive(pid):
            return True
        time.sleep(0.1)
    return False


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-folder-bundles-"))
    bad: list[str] = []
    pids: dict[str, int] = {}

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def short(x) -> str:
        return json.dumps(x, default=str)[:300]

    # ── before boot: the folders and one seeded instance of `cold` ──
    bundle(work, "cold", spec={"notes": NOTES})
    bundle(work, "broken", raw="base: py_agent\nsentence: [unclosed\n  verbs: {\n")
    bundle(work, "nosentence", raw=f"base: py_agent\ncommand: [{json.dumps(PY)}, agent.py]\n"
           "verbs: {echo: 'args: text:str.'}\n")
    bundle(work, "reserved", verbs={"echo": ECHO_DOC, "reflect": "No args. Mine."},
           extra=", 'reflect': ps")
    bundle(work, "sysverb", verbs={"echo": ECHO_DOC, "ps": "No args. The program's ps."},
           extra=", 'ps': ps")
    seeded = work / ".fantastic/agents/web/agents/rest/agents/cold1"
    seeded.mkdir(parents=True)
    (seeded / "agent.yaml").write_text(yaml.safe_dump(
        {"id": "cold1", "handler_module": "cold", "parent_id": "rest",
         "callable_by": {"$self": "*", "$parent": "*"}}, sort_keys=False))

    hk = HostKernel(spec, work, reach=["cold1", "c2", "h1", "s1", "x_broken", "p0"]).boot()
    root = hk.root_id
    cb = {"$self": "*", "$parent": "*"}

    def send(target: str, payload: dict) -> dict:
        r = hk.send(target, payload, timeout=CALL_S)
        return r if isinstance(r, dict) else {"_reply": r}

    def catalog() -> dict:
        r = send("kernel", {"type": "reflect", "tree": "none", "bundles": "all"})
        return {b.get("name"): b for b in r.get("bundles") or [] if "folder" in b}

    try:
        # 1 · cold load: the seeded instance answers right after boot
        e = send("cold1", {"type": "echo", "text": "a"})
        need(e.get("echo") == "v1:a" and isinstance(e.get("pid"), int)
             and e.get("pid") != os.getpid(),
             f"a bundle present at boot cold-loads; its seeded instance answers: {short(e)}")
        pids["cold1"] = e.get("pid")

        # 2 · catalog + docs
        cat = catalog()
        row = cat.get("cold") or {}
        need(row.get("base") == "py_agent" and row.get("handler_module") == "cold"
             and row.get("sentence") == "The cold test agent." and "error" not in row
             and str(row.get("folder", "")).endswith("bundles/cold"),
             f"reflect bundles=all lists the folder: {short(row)}")
        ids = send("kernel", {"type": "reflect", "tree": "none", "bundles": "ids"})
        need("cold" in (ids.get("bundles") or []),
             f"reflect bundles=ids names it: {short(ids.get('bundles'))}")
        d = send("kernel", {"type": "docs", "of": "cold"})
        need((d.get("verbs") or {}) == {"echo": "args: text:str (req)"},
             f"docs of=<bundle>: bundle.yaml's signature: {short(d)}")
        dd = send("kernel", {"type": "docs", "of": "cold", "tier": "detailed"})
        need((dd.get("verbs") or {}).get("echo") == ECHO_DOC
             and NOTES in str(dd.get("notes", "")),
             f"docs detailed: the full doc + the bundle's notes: {short(dd)}")
        di = send("kernel", {"type": "docs", "of": "cold1"})
        need((di.get("verbs") or {}) == {"echo": "args: text:str (req)"},
             f"docs of=<instance>: the same table: {short(di)}")

        # 3 · create an instance; reflect it; delete kills its program
        rec = hk.create(parent_id="rest", handler_module="cold", id="c2", callable_by=cb)
        need(rec.get("id") == "c2", f"create_agent handler_module=cold: {short(rec)}")
        e2 = send("c2", {"type": "echo", "text": "b"})
        need(e2.get("echo") == "v1:b" and e2.get("pid") not in (None, pids["cold1"]),
             f"its verb answers from its own program: {short(e2)}")
        rf = send("c2", {"type": "reflect"})
        need(rf.get("base") == "py_agent" and rf.get("bundle") == "cold"
             and rf.get("program") == "running" and rf.get("sentence") == "The cold test agent.",
             f"instance reflect: base, bundle, running program: {short(rf)}")
        un = send("c2", {"type": "nope"})
        need(un.get("reason") == "unhandled", f"an undeclared verb is unhandled: {short(un)}")
        dl = send(root, {"type": "delete_agent", "id": "c2"})
        need(not dl.get("error"), f"delete_agent c2: {short(dl)}")
        need(gone(e2.get("pid")), f"delete kills the program (pid {e2.get('pid')})")
        base = send(root, {"type": "create_agent", "parent_id": "rest",
                           "handler_module": "py_agent.tools", "id": "p0", "callable_by": cb})
        need(base.get("reason") == "invalid_meta" and "base" in str(base.get("error")),
             f"py_agent itself is a base, not an agent: {short(base)}")

        # 4 · hot load: a new folder, an edited program, a failed reload
        folder = bundle(work, "hot")
        hr = send("kernel", {"type": "reload_bundle", "name": "hot"})
        need(hr == {"reloaded": "hot", "instances": []},
             f"reload_bundle loads a folder added after boot: {short(hr)}")
        need((catalog().get("hot") or {}).get("base") == "py_agent",
             "the hot folder is listed")
        rec = hk.create(parent_id="rest", handler_module="hot", id="h1", callable_by=cb)
        h = send("h1", {"type": "echo", "text": "c"})
        need(rec.get("id") == "h1" and h.get("echo") == "v1:c",
             f"an instance of the hot folder answers: {short(h)}")
        (folder / "agent.py").write_text(program("v2"))
        hr2 = send("kernel", {"type": "reload_bundle", "name": "hot"})
        need(hr2 == {"reloaded": "hot", "instances": ["h1"]},
             f"reload names the live instance it restarts: {short(hr2)}")
        h2 = send("h1", {"type": "echo", "text": "c"})
        need(h2.get("echo") == "v2:c" and h2.get("pid") not in (None, h.get("pid")),
             f"the edited program answers the EXISTING instance (new pid): {short(h2)}")
        need(gone(h.get("pid")), f"the replaced program is gone (pid {h.get('pid')})")
        (folder / "bundle.yaml").write_text("base: py_agent\n")
        hr3 = send("kernel", {"type": "reload_bundle", "name": "hot"})
        need(hr3.get("reason") == "invalid_meta" and "sentence" in str(hr3.get("error")),
             f"a reload of a malformed bundle.yaml is refused naming the field: {short(hr3)}")
        h3 = send("h1", {"type": "echo", "text": "d"})
        need(h3 == {"echo": "v2:d", "pid": h2.get("pid")},
             f"the live instance keeps the previous load: {short(h3)}")
        need("error" not in (catalog().get("hot") or {"error": 1}),
             "the catalog keeps the previous load too")
        pids["h1"] = h2.get("pid")
        nf = send("kernel", {"type": "reload_bundle", "name": "no_such_folder"})
        need(nf.get("reason") == "not_found", f"reload of no folder: {short(nf)}")

        # 5 · not fatal: malformed folders are listed with their error
        cat = catalog()
        br = cat.get("broken") or {}
        need("does not parse" in str(br.get("error")) and "base" not in br,
             f"an unparsable bundle.yaml is listed with its error: {short(br)}")
        ns = cat.get("nosentence") or {}
        need("sentence: required" in str(ns.get("error")),
             f"a bundle.yaml without sentence is listed with its error: {short(ns)}")
        xb = hk.create(parent_id="rest", handler_module="broken", id="x_broken", callable_by=cb)
        xr = send("x_broken", {"type": "echo", "text": "x"})
        need(xr.get("reason") == "unhandled",
             f"an instance of a refused folder answers unhandled: create={short(xb)} call={short(xr)}")

        # 6 · reserved verbs cannot be shadowed
        rs = cat.get("reserved") or {}
        need("reflect" in str(rs.get("error")) and "answered by the kernel" in str(rs.get("error")),
             f"a bundle declaring `reflect` is refused: {short(rs)}")
        sv = cat.get("sysverb") or {}
        need(sv.get("base") == "py_agent" and "error" not in sv,
             f"a bundle declaring the kernel verb `ps` loads: {short(sv)}")
        rec = hk.create(parent_id="rest", handler_module="sysverb", id="s1", callable_by=cb)
        s = send("s1", {"type": "echo", "text": "s"})
        need(s.get("echo") == "v1:s", f"its declared verb answers: {short(s)}")
        p = send("s1", {"type": "ps"})
        need(p.get("reason") == "not_here" and not p.get("from_program"),
             f"`ps` on the instance is the kernel's answer, never the program's: {short(p)}")
        sr = send("s1", {"type": "reflect"})
        need(sr.get("base") == "py_agent" and sr.get("bundle") == "sysverb",
             f"`reflect` on the instance is the kernel's: {short(sr)}")
        pids["s1"] = s.get("pid")
    finally:
        hk.close()

    # 7 · the kernel stopped: every program with it
    for aid, pid in pids.items():
        need(gone(pid), f"{aid}'s program is gone after the kernel stops (pid {pid})")
    for pid in pids.values():  # never leave one behind, whatever the verdict
        if alive(pid):
            oslayer.kill_group(pid)
    shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
