"""Shared pieces of the export/import e2e tests (#268): a three-level team of real,
key-free host agents seeded under the root view's backend folder, the checks every
export folder must pass, and the record comparison a round trip needs.

The team is `lead` > `mid` > `leaf` (exec bridges: they load without a model):
  - `lead` can call `mid` and, OUTSIDE the subtree, `files` (it must become a port);
  - `mid` can call `leaf` (an inside grant: it must stay an id);
  - every record carries something that must NOT reach the folder: a key value, a
    runtime field, row text and (when asked) an absolute path."""

from __future__ import annotations

import re
import sys
from pathlib import Path
from typing import Any

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
from canvas_live import FOLDER  # noqa: E402

SECRET = "sk-e2e-SECRET"
ROW = "the e2e conversation row"
TEAM = ("lead", "mid", "leaf")
#: where the team lives: lead under the root view's backend folder, mid under lead, leaf under mid
LEAD_AT = FOLDER
MID_AT = f"{FOLDER}/agents/lead"
LEAF_AT = f"{MID_AT}/agents/mid"
#: the outside agent the team reaches (the project's own file bridge)
OUTSIDE = "files"


def _exec(agent_id: str, **extra: Any) -> dict:
    """A real exec bridge record: loads with no model and no key."""
    return {
        "id": agent_id, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
        "ingress_rule": "allow_all_ingress", "allowed": ["^echo e2e$"], "can_call": {}, "callable_by": {},
        "api_key": SECRET, "guid": "guid-" + agent_id, "started_at": "2026-01-01T00:00:00Z",
        "history": [ROW], "rows": [{"text": ROW}], **extra,
    }


def team_records(abs_path: str | None = None, extra: dict[str, dict] | None = None) -> dict[str, dict]:
    """The three records. `abs_path`: an absolute path put on `lead` (it must become a port)."""
    extra = extra or {}
    lead = _exec("lead", display_name="lead", can_call={"mid": "*", OUTSIDE: ["list"]}, **({"workdir": abs_path} if abs_path else {}), **extra.get("lead", {}))
    mid = _exec("mid", display_name="mid", can_call={"leaf": "*"}, callable_by={"lead": "*"}, **extra.get("mid", {}))
    leaf = _exec("leaf", display_name="leaf", callable_by={"mid": "*"}, notes="the bottom of the team", **extra.get("leaf", {}))
    return {"lead": lead, "mid": mid, "leaf": leaf}


def team_agents(abs_path: str | None = None, extra: dict[str, dict] | None = None, parent_name: str | None = "newparent") -> list[tuple[str, dict]]:
    """The team (and an empty `newparent` an import goes under) as `Rig.init(host_agents=…)` takes them:
    `(parent folder, record)`, seeded while the host is stopped."""
    recs = team_records(abs_path, extra)
    out = [(LEAD_AT, recs["lead"]), (MID_AT, recs["mid"]), (LEAF_AT, recs["leaf"])]
    if parent_name:
        out.append((FOLDER, _exec(parent_name, display_name=parent_name)))
    return out


def strip(rec: dict) -> dict:
    """A record without what a blueprint never carries (runtime state, keys, rows)."""
    drop = {"parent_id", "guid", "pid", "version", "rows", "history", "cache", "usage", "created_at", "updated_at", "ts",
            "last_error", "error", "agents", "api_key", "token", "secret", "password"}
    return {k: v for k, v in rec.items() if k not in drop and not k.startswith("last_") and not re.search(r"(_error|_at|_ts)$", k)}


def walk(agents: list, depth: int = 0):
    """Every `(entry, depth)` of the blueprint's nested `agents:` lists."""
    for a in agents or []:
        yield a, depth
        yield from walk(a.get("agents"), depth + 1)


def load_blueprint(folder: Path) -> dict:
    return yaml.safe_load((folder / "blueprint.yaml").read_text()) or {}


def every_file(folder: Path) -> list[Path]:
    return sorted(p for p in folder.rglob("*") if p.is_file())


def check_folder(folder: Path, proj: Path, ids: tuple[str, ...] = TEAM, outside: str = OUTSIDE, abs_path: str | None = None) -> dict[str, bool]:
    """The checks an export folder must pass; each name -> held."""
    r: dict[str, bool] = {}
    bp = folder / "blueprint.yaml"
    r["blueprint.yaml exists"] = bp.is_file()
    try:
        doc = load_blueprint(folder)
    except Exception:  # noqa: BLE001 - a parse error is a failed check
        doc = {}
    r["blueprint parses to a map with agents"] = isinstance(doc, dict) and bool(doc.get("agents"))
    entries = list(walk(doc.get("agents") or []))
    by_id = {e["id"]: (e, d) for e, d in entries if isinstance(e, dict) and "id" in e}
    r["the three agents are all there"] = set(ids) <= set(by_id)
    r["nested three levels deep, in order"] = [by_id.get(i, ({}, -1))[1] for i in ids] == list(range(len(ids)))
    r["no parent_id anywhere"] = all("parent_id" not in e for e, _ in entries)
    ports = doc.get("ports") or {}
    mid_id = ids[1]
    lead = by_id.get(ids[0], ({}, 0))[0]
    cc = lead.get("can_call") or {}
    r["the inside grant is by id"] = mid_id in cc
    # the host adds its own baseline grants (`user`, `canvas_root`…) at boot: they are ports too (`@connector:<id>`)
    r["the outside grant is a declared port @<id>, not the id"] = outside not in cc and f"@{outside}" in cc and f"@{outside}" in ports
    r["every other port is a declared connector"] = all(k in ports and k.startswith("@connector:") for k in cc if k.startswith("@") and k != f"@{outside}")
    r["no runtime field in a record"] = all(not re.search(r"^(guid|history|rows)$|_at$", k) for e, _ in entries for k in e)
    blob = "\n".join(p.read_text(errors="replace") for p in every_file(folder))
    r["no key value in any file"] = SECRET not in blob and "api_key" not in blob
    r["no row text in any file"] = ROW not in blob
    r["no absolute project path in any file"] = str(proj) not in blob and (abs_path is None or abs_path not in blob)
    return r


def failed(checks: dict[str, bool]) -> list[str]:
    return [k for k, v in checks.items() if not v]
