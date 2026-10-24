"""THE METER REPORT — one row per test from a run's `meter.jsonl` (helpers/meter.py).

    python3 e2e/helpers/meter_report.py e2e/runs/<stamp> [e2e/runs/<stamp> …]

Per test: verdict (SUMMARY.txt), model requests (turns, sub-AIs included), prompt
tokens summed / peak, completion tokens, model time (sum of request durations),
mean time to first byte, the wall span from the first request to the last, and the
dollar cost. Prompt and completion counts are the endpoint's own `usage`; a request
that streamed no usage (cut, timed out) is counted in `cut`.

THE COST is computed from the token totals and the prices in the environment — no
model name in code. `ANTHROPIC_PRICE_INPUT_PER_MTOK` and
`ANTHROPIC_PRICE_OUTPUT_PER_MTOK` (USD per million tokens) give the base rate;
`ANTHROPIC_PRICE_CACHE_READ_PER_MTOK` and `ANTHROPIC_PRICE_CACHE_WRITE_PER_MTOK`
the cache tiers (unset: those tokens bill at the input rate). A model with a
long-context tier names it with `ANTHROPIC_LONG_CONTEXT_THRESHOLD_TOKENS` (a request
whose prompt is above it) and `ANTHROPIC_PRICE_INPUT_LONG_PER_MTOK` /
`ANTHROPIC_PRICE_OUTPUT_LONG_PER_MTOK`; the report then says how many requests were
priced at that tier. Unset base prices: no cost column.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path


def _price_env() -> dict[str, float | int | None]:
    def num(var: str, cast=float):
        v = os.environ.get(var)
        return cast(v) if v not in (None, "") else None

    return {
        "in": num("ANTHROPIC_PRICE_INPUT_PER_MTOK"),
        "out": num("ANTHROPIC_PRICE_OUTPUT_PER_MTOK"),
        "read": num("ANTHROPIC_PRICE_CACHE_READ_PER_MTOK"),
        "write": num("ANTHROPIC_PRICE_CACHE_WRITE_PER_MTOK"),
        "long_at": num("ANTHROPIC_LONG_CONTEXT_THRESHOLD_TOKENS", int),
        "in_long": num("ANTHROPIC_PRICE_INPUT_LONG_PER_MTOK"),
        "out_long": num("ANTHROPIC_PRICE_OUTPUT_LONG_PER_MTOK"),
    }


def request_usd(r: dict, prices: dict) -> tuple[float | None, bool]:
    """Dollars for one request, and whether it was priced at the long-context tier.
    The prompt is input + cache write + cache read (Anthropic) or the prompt with its
    cached part (OpenAI shape): the uncached input is what is left after both."""
    if prices["in"] is None or prices["out"] is None or r.get("prompt_tokens") is None:
        return None, False
    pt = r["prompt_tokens"] or 0
    ct = r.get("completion_tokens") or 0
    cw = r.get("cache_write_tokens") or 0
    cr = r.get("cached_tokens") or 0
    long = bool(prices["long_at"] and prices["in_long"] is not None and pt > prices["long_at"])
    inp, out = (prices["in_long"], prices["out_long"]) if long else (prices["in"], prices["out"])
    if long and prices["out_long"] is None:
        out = prices["out"]
    rd = prices["read"] if prices["read"] is not None else inp
    wr = prices["write"] if prices["write"] is not None else inp
    usd = ((pt - cw - cr) * inp + cr * rd + cw * wr + ct * out) / 1_000_000
    return usd, long


def rows(run: Path) -> dict[str, dict]:
    prices = _price_env()
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
                "usd": 0.0,
                "usd_known": False,
                "long": 0,
            },
        )
        t["req"] += 1
        pt, ct = r.get("prompt_tokens"), r.get("completion_tokens")
        usd, long = request_usd(r, prices)
        if usd is not None:
            t["usd"] += usd
            t["usd_known"] = True
        t["long"] += long
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
    priced = any(t["usd_known"] for t in allrows.values())
    usd_head = f" {'usd':>8}" if priced else ""
    head = (
        f"{'test':<40} {'verdict':<7} {'req':>4} {'cut':>3} {'prompt_tok':>11} "
        f"{'peak':>7} {'compl_tok':>9} {'model_s':>8} {'span_s':>7} {'ttfb':>5}{usd_head}"
    )
    print(head)
    print("-" * len(head))
    tot = {"req": 0, "prompt": 0, "compl": 0, "model_s": 0.0, "usd": 0.0, "long": 0}
    for tag in sorted(allrows):
        t = allrows[tag]
        usd_cell = f" {t['usd']:>8.2f}" if priced else ""
        print(
            f"{tag:<40} {t['verdict']:<7} {t['req']:>4} {t['cut']:>3} {t['prompt']:>11,} "
            f"{t['peak']:>7,} {t['compl']:>9,} {t['model_s']:>8.0f} {t['span_s']:>7.0f} "
            f"{t['ttfb_mean']:>5.1f}{usd_cell}"
        )
        for k in tot:
            tot[k] += t[k]
    print("-" * len(head))
    usd_total = f" {tot['usd']:>8.2f}" if priced else ""
    print(
        f"{'TOTAL':<40} {'':<7} {tot['req']:>4} {'':>3} {tot['prompt']:>11,} {'':>7} "
        f"{tot['compl']:>9,} {tot['model_s']:>8.0f}{usd_total}"
    )
    if priced:
        prices = _price_env()
        print(f"\ncost: USD, from ANTHROPIC_PRICE_* in this environment "
              f"(input {prices['in']}, output {prices['out']} per MTok)")
        if prices["read"] is None or prices["write"] is None:
            print("      cache tiers without a price bill at the input rate")
        if prices["long_at"]:
            print(f"      long-context tier above {prices['long_at']:,} prompt tokens: "
                  f"{tot['long']} request(s) priced at it")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
