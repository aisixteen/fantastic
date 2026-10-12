"""THE METER REPORT — one row per test from a run's `meter.jsonl` (helpers/meter.py).

    python3 e2e/helpers/meter_report.py e2e/runs/<stamp> [e2e/runs/<stamp> …]

Per test: verdict (SUMMARY.txt), model requests (turns, sub-AIs included), prompt
tokens summed / peak, completion tokens, model time (sum of request durations),
mean time to first byte, and the wall span from the first request to the last.
Prompt and completion counts are the endpoint's own `usage`; a request that
streamed no usage (cut, timed out) is counted in `cut`.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def rows(run: Path) -> dict[str, dict]:
    verdicts = {}
    summary = run / "SUMMARY.txt"
    if summary.exists():
        for line in summary.read_text().splitlines():
            parts = line.split("\t")
            if len(parts) >= 2:
                verdicts[parts[1]] = parts[0]
    out: dict[str, dict] = {}
    for line in (run / "meter.jsonl").read_text().splitlines():
        r = json.loads(line)
        t = out.setdefault(
            r["tag"],
            {
                "req": 0,
                "cut": 0,
                "prompt": 0,
                "peak": 0,
                "compl": 0,
                "model_s": 0.0,
                "ttfb": [],
                "t0": r["t0"],
                "t1": r["t0"],
            },
        )
        t["req"] += 1
        pt, ct = r.get("prompt_tokens"), r.get("completion_tokens")
        if pt is None:
            t["cut"] += 1
        else:
            t["prompt"] += pt
            t["peak"] = max(t["peak"], pt)
            t["compl"] += ct or 0
        t["model_s"] += r.get("dur") or 0.0
        if r.get("ttfb") is not None:
            t["ttfb"].append(r["ttfb"])
        t["t0"] = min(t["t0"], r["t0"])
        t["t1"] = max(t["t1"], r["t0"] + (r.get("dur") or 0.0))
    for tag, t in out.items():
        t["verdict"] = verdicts.get(tag, "?")
        t["span_s"] = t["t1"] - t["t0"]
        t["ttfb_mean"] = sum(t["ttfb"]) / len(t["ttfb"]) if t["ttfb"] else 0.0
    return out


def main(argv: list[str]) -> int:
    allrows: dict[str, dict] = {}
    for a in argv:
        allrows.update(rows(Path(a)))
    head = (
        f"{'test':<40} {'verdict':<7} {'req':>4} {'cut':>3} {'prompt_tok':>11} "
        f"{'peak':>7} {'compl_tok':>9} {'model_s':>8} {'span_s':>7} {'ttfb':>5}"
    )
    print(head)
    print("-" * len(head))
    tot = {"req": 0, "prompt": 0, "compl": 0, "model_s": 0.0}
    for tag in sorted(allrows):
        t = allrows[tag]
        print(
            f"{tag:<40} {t['verdict']:<7} {t['req']:>4} {t['cut']:>3} {t['prompt']:>11,} "
            f"{t['peak']:>7,} {t['compl']:>9,} {t['model_s']:>8.0f} {t['span_s']:>7.0f} "
            f"{t['ttfb_mean']:>5.1f}"
        )
        for k in tot:
            tot[k] += t[k]
    print("-" * len(head))
    print(
        f"{'TOTAL':<40} {'':<7} {tot['req']:>4} {'':>3} {tot['prompt']:>11,} {'':>7} "
        f"{tot['compl']:>9,} {tot['model_s']:>8.0f}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
