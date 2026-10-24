"""PREFLIGHT for the live probes — checks, never calls a model.

Run this before any `tmp/live_*.py`. It answers one question: would a probe fail
for a REASON THAT IS NOT THE SUBSTRATE? A live run that dies on a missing model,
an unset variable or a backend nobody started tells you nothing about the kernel,
and costs a turn to work out.

    cd src/lib/python && uv run python ../../../tmp/live_preflight.py

Exits 0 when a probe could run, 1 otherwise, and NAMES the fix. It never sends a
completion: the point is to be free and instant.

NO DEFAULTS, deliberately. This refuses to guess a model, an endpoint or a
backend — the same rule the kernel holds itself to. A guessed default is how a
run ends up measuring the wrong thing.
"""

from __future__ import annotations

import os
import socket
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent

OK, BAD, WARN = "  ok ", "  NO ", " warn"


def _listening(host: str, port: int) -> bool:
    with socket.socket() as s:
        s.settimeout(0.4)
        return s.connect_ex((host, port)) == 0


def _which(binary: str) -> str | None:
    try:
        out = subprocess.run(
            ["which", binary], capture_output=True, text=True, timeout=5
        )
        return out.stdout.strip() or None
    except Exception:
        return None


def main() -> int:
    # a Windows console is cp1252, which has no box-drawing characters
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    print("live-probe preflight — nothing is sent to a model\n")
    blockers: list[str] = []
    notes: list[str] = []

    # ── which backend is being asked for ────────────────────────
    if str(HERE) not in sys.path:
        sys.path.insert(0, str(HERE))
    from backends import (
        REGISTRY,
        anthropic_endpoint_problem,
        backend as resolve_backend,
    )
    from helpers.creds import KEYED

    backend = os.environ.get("BACKEND")
    if not backend:
        print(f"{WARN} BACKEND unset — each probe falls back to its OWN default")
        print("       (several still default to `ollama`, which is not installed)")
        notes.append("set BACKEND explicitly: " + " | ".join(sorted(REGISTRY)))
    elif backend not in REGISTRY:
        print(f"{BAD} BACKEND={backend} is not a plug")
        blockers.append(f"BACKEND={backend} is unknown — one of {' | '.join(sorted(REGISTRY))}")
    else:
        print(f"{OK} BACKEND={backend}")
        # THE PLUG names what it needs (its endpoint, its required fields, its
        # key) — asked here, never restated, so the two cannot drift. A plug that
        # cannot be built (no model, a bad caching mode) names that instead.
        try:
            plug = resolve_backend(backend)
        except SystemExit as why:
            plug = None
            if "FANTASTIC_AI_MODEL is unset" not in str(why):
                blockers.append(str(why))
        if plug is not None:
            blockers += plug.blockers()
        elif backend == "anthropic":
            # no model yet: the key and the endpoint are still named, not hidden behind it
            for var in KEYED["anthropic"]:
                if not os.environ.get(var):
                    blockers.append(f"{var} is unset — the plug needs it")
        if backend == "anthropic" and os.environ.get("ANTHROPIC_ENDPOINT"):
            if problem := anthropic_endpoint_problem(os.environ["ANTHROPIC_ENDPOINT"]):
                blockers.append(problem)

    # ── CONCURRENCY: the GPU is shared ──────────────────────────
    # The vLLM GPU holds a fixed number of sequences and production
    # traffic shares it, so exceeding the cap degrades somebody else's work, not
    # just ours. It is a promise about another team's latency, not a throughput
    # dial.
    cap = int(os.environ.get("LANE_CAP", "6"))
    lanes = int(os.environ.get("LANES", "1"))
    print(f"{OK if lanes <= cap else BAD} LANES={lanes:<15} "
          f"{f'within the {cap}-lane cap' if lanes <= cap else f'OVER the agreed cap of {cap}'}")
    if lanes > cap:
        blockers.append(f"LANES={lanes} exceeds the agreed cap of {cap} concurrent lanes")

    # ── the keys: read by the run's `creds` store (env backend), never a backend ──
    for var, _endpoint_var in KEYED.values():
        have = bool(os.environ.get(var))
        print(f"{OK if have else WARN} {var:<20} {'set' if have else 'unset'}")

    # ── ollama, which several probes still default to ───────────
    if not _which("ollama") and not _listening("127.0.0.1", 11434):
        print(f"{WARN} ollama               not installed, nothing on :11434")
        notes.append(
            "probes defaulting to BACKEND=ollama cannot run — pass BACKEND "
            "explicitly or they fail on connect, not on the substrate"
        )

    # ── what the probes read ────────────────────────────────────
    for var in ("FANTASTIC_AI_MODEL", "FANTASTIC_AI_SEND_TIMEOUT", "CTX"):
        v = os.environ.get(var)
        print(f"{OK if v else WARN} {var:<26} {v or '(unset — probe default applies)'}")
    if backend and not os.environ.get("FANTASTIC_AI_MODEL"):
        msg = (
            "FANTASTIC_AI_MODEL is unset — the substrate refuses to guess a model, "
            "and so does this"
        )
        if msg not in blockers:
            blockers.append(msg)

    # ── the probes themselves ───────────────────────────────────
    probes = sorted(HERE.glob("live_*.py"))
    probes = [p for p in probes if p.name != "live_preflight.py"]
    print(f"\n{OK} {len(probes)} probes present under tmp/")

    print("\n" + "─" * 62)
    for n in notes:
        print(f"note: {n}")
    if blockers:
        print("\nNOT READY:")
        for b in blockers:
            print(f"  · {b}")
        return 1
    print("\nREADY — a probe would reach the model.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
