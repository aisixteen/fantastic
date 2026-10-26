"""E128 export_tui — the TUI's `/export <path>` command (#268).

The real `fantastic` TUI in a pseudo-terminal (the `screenshot` harness,
`src/tui/e2e/screenshot.rs`) over a project whose `.fantastic` holds the seeded
team (`lead` > `mid` > `leaf`, written the way the product stores agents, under `user`).

a) the tree lists `lead`; the keys select it; `/export out/tui` + Enter writes
   `out/tui` — a blueprint folder (nested, ports, no key / runtime field / row
   text); the composer is emptied and the screen shows no refusal (the TUI notes
   only refusals);
b) `/export ../x` shows "export: path may not hold `..`" and no `../x` folder exists;
c) `/export` with no path shows its usage and writes nothing.

No PTY (Windows): the test prints E2E-SKIP and ends."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import E2E_TARGET, SRC, Rig, build_lock, oslayer, verdict  # noqa: E402
from _lib.export_live import check_folder, team_agents  # noqa: E402

#: the team sits under `user` in the TUI's own tree
PARENT = "agents/user"


def harness() -> Path:
    """The PTY harness binary: `FANTASTIC_SCREENSHOT`, or `FANTASTIC_TUI_RIG` (what
    `E2E_ARTIFACTS` sets, so a CI run builds nothing here), else built into the e2e target."""
    for var in ("FANTASTIC_SCREENSHOT", "FANTASTIC_TUI_RIG"):
        if os.environ.get(var):
            return Path(os.environ[var]).expanduser().resolve()
    with build_lock(E2E_TARGET):
        r = subprocess.run(["cargo", "build", "--example", "screenshot", "-p", "fantastic-tui"], cwd=SRC, capture_output=True, text=True,
                           env={**os.environ, "CARGO_TARGET_DIR": str(E2E_TARGET)})
        if r.returncode != 0:
            raise SystemExit(f"building the screenshot harness failed:\n{r.stderr[-2000:]}")
    return oslayer.exe(E2E_TARGET / "debug" / "examples" / "screenshot")


def play(rig: Rig, shot: Path, name: str, script: str) -> dict[str, str]:
    """Run `script` in the PTY harness over the project; the frames it shot, by label."""
    sf = rig.art / f"{name}.script"
    sf.write_text(script)
    out = rig.art / f"frames_{name}"
    r = subprocess.run([str(shot), str(rig.binary), str(sf), str(out)], capture_output=True, text=True, timeout=180,
                       env={**os.environ, "FT_CWD": str(rig.proj), "FT_ROWS": "40", "FT_COLS": "130"})
    (rig.art / f"{name}.out").write_text(r.stdout + r.stderr)
    return {re.sub(r"^\d+_", "", p.stem): p.read_text() for p in sorted(out.glob("*.txt"))}


def left_rows(frame: str) -> list[str]:
    """The tree pane's rows, top to bottom (the first column of the frame, after the border)."""
    rows = []
    for ln in frame.splitlines()[2:]:
        m = re.match(r"^│(.{26})│", ln)
        if not m:
            break
        rows.append(m.group(1))
    return rows


def main() -> bool:
    rig = Rig(__file__, live=False)
    if os.name == "nt":
        print("E2E-SKIP: no pseudo-terminal harness on Windows")
        return True
    rig.build()
    shot = harness()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "out").mkdir()
    r: dict = {}
    info: dict = {}
    # a first boot makes the project's .fantastic; the team is seeded after it
    first = play(rig, shot, "boot", "until 30000 user\nshot boot\n")
    r["0 the TUI boots over the project"] = "boot" in first and (rig.proj / ".fantastic" / "agents" / "user").is_dir()
    for parent, rec in team_agents(parent_name=None):
        # the team's own folder is `lead`'s: `team_agents` places it for the canvas; here it sits under `user`
        rel = parent.replace("agents/user/agents/web/agents/canvas_page/agents/canvas_root", PARENT)
        rig.seed_host_agent(rel, rec)
    tree = play(rig, shot, "tree", "until 30000 lead\nshot tree\n")
    rows = left_rows(tree.get("tree", ""))
    idx = next((i for i, row in enumerate(rows) if re.search(r"\blead\b", row)), -1)
    info["tree_rows"] = rows
    info["lead_row"] = idx
    r["1 the tree lists lead"] = idx > 0
    downs = "\n".join(["key down"] * max(idx, 0))
    frames = play(rig, shot, "export", f"until 30000 lead\n{downs}\nkey enter\nwait 800\nshot selected\nkey tab\ntype /export out/tui\nkey enter\nwait 2500\nshot done\n")
    info["done"] = frames.get("done", "")[-2500:]
    out = rig.proj / "out" / "tui"
    for k, v in check_folder(out, rig.proj).items():
        r[f"a folder: {k}"] = v
    r["a1 the selected agent is lead"] = "lead  ● open" in frames.get("selected", "")
    # the TUI notes only a refusal; a written export shows as the emptied composer (and the folder)
    r["a2 the command was taken: the composer is empty and shows no refusal"] = "Enter sends" in frames.get("done", "") and "export:" not in frames.get("done", "")
    # b) a path with .. is refused on screen
    bad = play(rig, shot, "dots", f"until 30000 lead\n{downs}\nkey enter\nwait 800\nkey tab\ntype /export ../x\nkey enter\nwait 1500\nshot bad\n")
    r["b1 the screen shows the refusal"] = "export: path may not hold `..`" in bad.get("bad", "")
    r["b2 and no ../x folder exists"] = not (rig.proj.parent / "x").exists() and not (rig.proj / "x").exists()
    # c) no path: its usage
    usage = play(rig, shot, "usage", f"until 30000 lead\n{downs}\nkey enter\nwait 800\nkey tab\ntype /export\nkey enter\nwait 1500\nshot usage\n")
    r["c1 no path shows the usage"] = "export: usage: /export <path>" in usage.get("usage", "")
    r["c2 and out holds only the one folder"] = sorted(p.name for p in (rig.proj / "out").iterdir()) == ["tui"]
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:3000])
    bad_ = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not bad_, f"{len(r) - len(bad_)}/{len(r)} pass" + ("" if not bad_ else f" — failed: {'; '.join(bad_)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
