"""kernel/recall_scale — a FRESH execution name vs the stored-call count. NO MODEL. Run SOLO.

On the PACKAGED host of `RUNTIME` (python | rust | swift), every call through
`web_rest`:

  1. FILL the kernel's memory store with `call` rows: `reflect tree=none` on a small
     agent (`pad`, a `ram_store` child of the `rest` leg), 16 client threads on
     keep-alive connections (a fresh TCP connection per call exhausts the client's
     ephemeral ports at this volume), up to each checkpoint in `CHECKPOINTS`. One
     REST call stores more than one `call` row (python: the leg's `inbound_call` and
     the `reflect` it dispatches), so the rows-per-call ratio is MEASURED once and
     the fill sized from it;
  2. COUNT the stored calls from the store itself: `dump` of `memory`, the kernel's
     own (`<guid>|kernel`) non-internal `call` rows — every one of them is a row
     `recall_executions` scans — polled until the flush has caught up;
  3. TIME `N_TIMED` calls each carrying a FRESH execution name (a uuid on
     `X-Fantastic-Execution-Id`) and `N_TIMED` unnamed calls, one at a time.

Checks: the count reached every checkpoint (nothing pruned below it), and the
BOUND at the last checkpoint: fresh-name median ≤ 3 × unnamed median + 5 ms.
"""

from __future__ import annotations

import http.client
import json
import math
import os
import shutil
import statistics
import sys
import tempfile
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import EXEC_ID_HEADER, HostKernel, ensure_runtime, runtime
from helpers.memory import MEMORY
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
PAD = "pad"
CHECKPOINTS = (0, 10_000, 50_000, 100_000)
N_TIMED = 20
THREADS = 16
#: Calls issued to measure how many `call` rows one REST call stores.
CALIBRATE = 500
#: Seconds to wait for the flush to catch up with the fill.
FLUSH_WAIT = 120.0


def ms_stats(xs: list[float]) -> tuple[float, float]:
    s = sorted(xs)
    p95 = s[min(len(s) - 1, round(0.95 * (len(s) - 1)))]
    return statistics.median(s) * 1000, p95 * 1000


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    bad: list[str] = []
    table: list[dict] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    work = Path(tempfile.mkdtemp(prefix="e2e-recall-scale-"))
    hk = HostKernel(spec, work, reach=[PAD])
    hk.boot()
    try:
        rec = hk.create(parent_id="rest", handler_module="ram_store.tools", id=PAD,
                        callable_by={"$self": "*", "$parent": "*"})
        need(rec.get("id") == PAD, f"create {PAD}: {str(rec)[:160]}")

        local = threading.local()

        def post(target: str, payload: dict, exec_id: str | None = None) -> dict:
            """`HostKernel.send`'s JSON path on a per-thread KEEP-ALIVE connection.
            The server closes an idle one, so a phase that follows idle time starts
            on a fresh connection (`fresh()`)."""
            conn = getattr(local, "conn", None)
            if conn is None:
                conn = local.conn = http.client.HTTPConnection("127.0.0.1", hk.port, timeout=600)
            headers = {"content-type": "application/json"}
            if exec_id is not None:
                headers[EXEC_ID_HEADER] = exec_id
            conn.request("POST", f"/rest/{target}", body=json.dumps(payload).encode(),
                         headers=headers)
            r = conn.getresponse()
            raw = r.read()
            if r.status != 200:
                return {"error": f"HTTP {r.status}", "body": raw.decode()[:300]}
            return json.loads(raw or b"null")

        def fresh() -> None:
            conn = getattr(local, "conn", None)
            if conn is not None:
                conn.close()
            local.conn = None

        def call(exec_id: str | None = None) -> float:
            t = time.monotonic()
            r = post(PAD, {"type": "reflect", "tree": "none"}, exec_id)
            dt = time.monotonic() - t
            if not isinstance(r, dict) or r.get("id") != PAD:
                raise RuntimeError(f"reflect {PAD} (exec_id={exec_id!r}): {str(r)[:200]}")
            return dt

        def stored_calls() -> tuple[int, float]:
            """The kernel's non-internal `call` rows in its memory store, from `dump`."""
            fresh()
            t = time.monotonic()
            r = post(MEMORY, {"type": "dump"})
            dt = time.monotonic() - t
            rows = (r.get("tables") or {}).get("rows")
            if not isinstance(rows, list):
                raise TypeError(f"memory dump: {str(r)[:200]}")
            n = sum(1 for x in rows
                    if str(x.get("owner", "")).split("|", 1)[-1] == "kernel"
                    and (x.get("row") or {}).get("kind") == "call"
                    and not (x.get("row") or {}).get("internal"))
            return n, dt

        def settled() -> tuple[int, float]:
            """Poll the store until the flush has caught up: two reads 1 s apart differ
            by at most the few rows the reads themselves add (or FLUSH_WAIT passes)."""
            end = time.monotonic() + FLUSH_WAIT
            n, dump_s = stored_calls()
            while time.monotonic() < end:
                time.sleep(1.0)
                m, dump_s = stored_calls()
                if m - n <= 4:
                    return m, dump_s
                n = m
            return n, dump_s

        def fill(k: int, cp: int) -> None:
            t, done = time.monotonic(), 0
            with ThreadPoolExecutor(THREADS) as ex:
                while done < k:
                    chunk = min(10_000, k - done)
                    list(ex.map(lambda _: call(), range(chunk)))
                    done += chunk
                    print(f"     fill {done}/{k} calls toward {cp} · "
                          f"{time.monotonic() - t:.0f}s", flush=True)

        def measure(cp: int, n: int, fill_calls: int, fill_s: float) -> None:
            fresh()
            call()  # opens the connection; not timed
            named = [call(f"recall-scale-{uuid.uuid4().hex}") for _ in range(N_TIMED)]
            plain = [call() for _ in range(N_TIMED)]
            fm, fp = ms_stats(named)
            um, up = ms_stats(plain)
            row = {"checkpoint": cp, "stored": n, "fresh_med_ms": round(fm, 2),
                   "fresh_p95_ms": round(fp, 2), "unnamed_med_ms": round(um, 2),
                   "unnamed_p95_ms": round(up, 2), "fill_s": round(fill_s, 1),
                   "fill_calls": fill_calls}
            table.append(row)
            print(f"  ROW {name}: " + json.dumps(row), flush=True)

        ratio = 0.0
        for cp in CHECKPOINTS:
            n, dump_s = settled()
            todo, t = 0, time.monotonic()
            if cp > n and not ratio:
                # one measured batch: how many stored rows one REST call makes
                fill(CALIBRATE, cp)
                n1, dump_s = settled()
                ratio = (n1 - n) / CALIBRATE
                print(f"     {ratio:.2f} stored call rows per REST call "
                      f"({n} → {n1} over {CALIBRATE})", flush=True)
                todo, n = CALIBRATE, n1
            for _ in range(5):  # top up: the ratio is a measurement, not exact
                if n >= cp:
                    break
                k = math.ceil((cp - n) / ratio)
                fill(k, cp)
                todo += k
                n, dump_s = settled()
            fill_s = time.monotonic() - t
            need(n >= cp, f"checkpoint {cp}: the store holds {n} kernel call rows "
                          f"(dump {dump_s:.2f}s; fill {todo} calls in {fill_s:.1f}s)")
            measure(cp, n, todo, fill_s)

        last = table[-1]
        bound = 3 * last["unnamed_med_ms"] + 5
        need(last["fresh_med_ms"] <= bound,
             f"BOUND at {last['checkpoint']}: fresh-name median {last['fresh_med_ms']:.2f} ms "
             f"≤ 3 × unnamed median {last['unnamed_med_ms']:.2f} ms + 5 = {bound:.2f} ms")
        # ABSOLUTE too: a store whose every call slows with its size would pass
        # the relative bound with both numbers large.
        first = table[0]
        flat = 3 * first["unnamed_med_ms"] + 5
        need(last["unnamed_med_ms"] <= flat,
             f"BOUND at {last['checkpoint']}: unnamed median {last['unnamed_med_ms']:.2f} ms "
             f"≤ 3 × the empty store's {first['unnamed_med_ms']:.2f} ms + 5 = {flat:.2f} ms")
    finally:
        hk.close()
        shutil.rmtree(work, ignore_errors=True)

    print(f"\n  TABLE {name}", flush=True)
    print("  checkpoint   stored  fresh med/p95 ms   unnamed med/p95 ms   fill s", flush=True)
    for r in table:
        print(f"  {r['checkpoint']:>10} {r['stored']:>8}  {r['fresh_med_ms']:>8.2f} /{r['fresh_p95_ms']:>8.2f}"
              f"   {r['unnamed_med_ms']:>8.2f} /{r['unnamed_p95_ms']:>8.2f}   {r['fill_s']:>6.1f}",
              flush=True)
    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
