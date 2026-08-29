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
    print("live-probe preflight — nothing is sent to a model\n")
    blockers: list[str] = []
    notes: list[str] = []

    # ── which backend is being asked for ────────────────────────
    backend = os.environ.get("BACKEND")
    if not backend:
        print(f"{WARN} BACKEND unset — each probe falls back to its OWN default")
        print("       (several still default to `ollama`, which is not installed)")
        notes.append("set BACKEND explicitly: llamacpp | anthropic | nvidia")
    else:
        print(f"{OK} BACKEND={backend}")

    # ── the local path: llama.cpp ───────────────────────────────
    server = _which("llama-server")
    endpoint = os.environ.get("FANTASTIC_LLAMACPP_ENDPOINT", "http://127.0.0.1:8080/v1")
    up = _listening("127.0.0.1", 8080)
    print(f"{OK if server else BAD} llama-server binary  {server or '(not on PATH)'}")
    print(f"{OK if up else BAD} llama-server on :8080 {endpoint if up else '(nothing listening)'}")

    models = Path.home() / "Projects" / "models"
    weights = sorted(models.glob("*.gguf")) if models.exists() else []
    print(f"{OK if weights else BAD} model weights        "
          f"{weights[0].name if weights else f'(no *.gguf under {models})'}")
    if backend in (None, "llamacpp") and not (server and up and weights):
        blockers.append(
            "llamacpp path is not ready — weights are NOT fetched automatically "
            "(never download a model on the tool's own initiative). Put a .gguf "
            f"under {models}, then: llama-server -m <file> --port 8080"
        )

    # ── the GPU proxy over the SSM tunnel ───────────────────────
    # Anthropic wire at /cc/v1/messages, raw passthrough at /raw. The probes
    # already read ENDPOINT for the anthropic path, so this needs no code change
    # — only the environment.
    tunnel = _listening("127.0.0.1", 4000)
    print(f"{OK if tunnel else BAD} GPU proxy :4000     "
          f"{'answering' if tunnel else '(tunnel down — SSM forwards have died mid-run before)'}")
    ep = os.environ.get("ENDPOINT")
    print(f"{OK if ep else WARN} ENDPOINT             {ep or '(unset — anthropic path would hit api.anthropic.com)'}")
    if backend == "anthropic":
        if not tunnel:
            blockers.append("BACKEND=anthropic with the tunnel down — restart the SSM forward")
        if not ep:
            blockers.append(
                "ENDPOINT unset: the record falls back to api.anthropic.com while "
                "carrying whatever key is exported, which 401s. Set "
                "ENDPOINT=http://127.0.0.1:4000/cc/v1/messages"
            )

    # ── CONCURRENCY: the GPU is shared ──────────────────────────
    # The GPU holds ~7 sequences and production production traffic shares it, so
    # exceeding the cap degrades somebody else's work, not just ours. Opened at 3
    # by the session that built the tunnel; raised to 6 by its owner 2026-08-28.
    # It is a promise about another team's latency, not a throughput dial.
    cap = int(os.environ.get("LANE_CAP", "6"))
    lanes = int(os.environ.get("LANES", "1"))
    print(f"{OK if lanes <= cap else BAD} LANES={lanes:<15} "
          f"{f'within the {cap}-lane cap' if lanes <= cap else f'OVER the agreed cap of {cap}'}")
    if lanes > cap:
        blockers.append(f"LANES={lanes} exceeds the agreed cap of {cap} concurrent lanes")

    # ── the paid paths ──────────────────────────────────────────
    for var, name in (("ANTHROPIC_API_KEY", "anthropic"), ("NVIDIA_API_KEY", "nvidia")):
        have = bool(os.environ.get(var))
        print(f"{OK if have else WARN} {var:<20} {'set' if have else 'unset'}")
        if backend == name and not have:
            blockers.append(
                f"BACKEND={name} needs {var} in the environment (or wired through "
                "the keychain connector)"
            )

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
        blockers.append(
            "FANTASTIC_AI_MODEL is unset — the substrate refuses to guess a model, "
            "and so does this"
        )

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
