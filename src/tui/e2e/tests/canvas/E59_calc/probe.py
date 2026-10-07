"""E59 calc — behind a SEEDED six-button panel an AI writes a multi-function
math module and its own tests; the harness runs both, its hidden vectors, and
every button."""

from __future__ import annotations

import asyncio
import json
import math
import subprocess
import sys
from fractions import Fraction
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, verdict, ask  # noqa: E402

FNS = ["det", "factor", "simpson", "roots", "fib_mod", "gcd_lcm"]

ASK = ("The panel `calc` (the exec agent `calc_exec`'s own view) has six buttons; each runs `python3 calc.py <fn> <json input>` on the exec agent `calc_exec` "
       "and shows the last line printed. Write calc.py (stdlib only) so every button works — each call prints ONE JSON line:\n"
       "- det: an integer matrix → its exact integer determinant\n"
       "- factor: an integer n ≥ 2 → [[p, e], …], primes ascending\n"
       "- simpson: {\"f\": a python expression in x (math names: sin, cos, exp, log, sqrt, pi), \"a\", \"b\", \"n\" (even)} "
       "→ the composite Simpson integral, a float\n"
       "- roots: polynomial coefficients, highest power first → its distinct real roots, ascending, floats\n"
       "- fib_mod: [n, m] with n up to 10^18 → F(n) mod m (F(0)=0, F(1)=1)\n"
       "- gcd_lcm: a list of integers → [gcd, lcm]\n"
       "Also write test_calc.py testing every function; `python3 test_calc.py` must exit 0. Run it.")


# ── the reference (hidden from the model) ────────────────────────────────────
def r_det(m):
    a = [[Fraction(x) for x in row] for row in m]
    n, d = len(a), Fraction(1)
    for c in range(n):
        p = next((r for r in range(c, n) if a[r][c] != 0), None)
        if p is None:
            return 0
        if p != c:
            a[c], a[p] = a[p], a[c]
            d = -d
        d *= a[c][c]
        for r in range(c + 1, n):
            f = a[r][c] / a[c][c]
            a[r] = [x - f * y for x, y in zip(a[r], a[c])]
    return int(d)


def r_factor(n):
    out, p = [], 2
    while p * p <= n:
        e = 0
        while n % p == 0:
            n //= p
            e += 1
        if e:
            out.append([p, e])
        p += 1 if p == 2 else 2
    if n > 1:
        out.append([n, 1])
    return out


def r_simpson(q):
    f = eval("lambda x: " + q["f"], {k: getattr(math, k) for k in ("sin", "cos", "exp", "log", "sqrt", "pi")})
    a, b, n = q["a"], q["b"], q["n"]
    h = (b - a) / n
    return h / 3 * (f(a) + f(b) + sum((4 if i % 2 else 2) * f(a + i * h) for i in range(1, n)))


def r_fib(q):
    n, m = q

    def fd(k):
        if k == 0:
            return 0, 1
        a, b = fd(k >> 1)
        c, d = a * (2 * b - a) % m, (a * a + b * b) % m
        return (d, (c + d) % m) if k & 1 else (c, d)

    return fd(n)[0] % m


def r_gcd_lcm(xs):
    return [math.gcd(*xs), math.lcm(*xs)]


ROOTS = {json.dumps([1, -6, 11, -6]): [1.0, 2.0, 3.0], json.dumps([2, 0, -8, 0]): [-2.0, 0.0, 2.0],
         json.dumps([1, 0, 1]): [], json.dumps([1, -3, 2]): [1.0, 2.0]}
REF = {"det": r_det, "factor": r_factor, "simpson": r_simpson, "roots": lambda c: ROOTS[json.dumps(c)],
       "fib_mod": r_fib, "gcd_lcm": r_gcd_lcm}

HIDDEN = {
    "det": [[[3, 141, -59, 26, 535, 89], [79, 323, -84, 62, 64, 33], [-83, 27, 950, 288, 41, 97],
             [169, -399, 37, 510, 58, 20], [97, 494, 459, -230, 78, 164], [62, 86, 20, 89, 98, -628]],
            [[1, 2, 3, 4], [2, 4, 6, 8], [0, 1, 5, 7], [3, 3, 3, 3]]],
    "factor": [600851475143, 2 ** 10 * 3 ** 5 * 1000003],
    "simpson": [{"f": "exp(-x*x)", "a": -2, "b": 2, "n": 1000}, {"f": "x**3*cos(x)+sqrt(x)", "a": 0, "b": 2, "n": 500}],
    "roots": [[1, -6, 11, -6], [2, 0, -8, 0], [1, 0, 1]],
    "fib_mod": [[10 ** 18, 10 ** 9 + 7], [90, 10 ** 12]],
    "gcd_lcm": [[12, 18, 30], [2 ** 40 * 3, 2 ** 20 * 35, 2 ** 30 * 11]],
}
# the buttons' own inputs — not the hidden ones
UI = {"det": [[2, 1], [1, 3]], "factor": 360, "simpson": {"f": "sin(x)", "a": 0, "b": math.pi, "n": 100},
      "roots": [1, -3, 2], "fib_mod": [100, 10 ** 9 + 7], "gcd_lcm": [4, 6, 8]}


def same(fn: str, got, want) -> bool:
    try:
        if fn == "simpson":
            return abs(float(got) - want) <= 1e-6 * max(1.0, abs(want))
        if fn == "roots":
            return len(got) == len(want) and all(abs(float(g) - w) <= 1e-4 for g, w in zip(got, want))
        return json.loads(json.dumps(got)) == json.loads(json.dumps(want))
    except (TypeError, ValueError):
        return False


def last_json(text: str):
    lines = [ln for ln in text.strip().splitlines() if ln.strip()]
    return json.loads(lines[-1]) if lines else None


def run(proj: Path, *args: str, timeout: float = 30) -> tuple[int, str]:
    try:
        p = subprocess.run(["python3", *args], cwd=proj, capture_output=True, text=True, timeout=timeout)
        return p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired:
        return -1, "timeout"


# ── the seed: the exec agent and the six-button panel ────────────────────────
def exec_record() -> dict:
    """ONE host agent (backend-owned views): the exec that may run exactly the
    calc lines, carrying its OWN view — the six-button panel, the verbs it calls."""
    return {"id": "calc_exec", "handler_module": "exec_bridge.tools", "parent_id": "canvas_root",
            "allowed": [r"^python3 calc\.py (det|factor|simpson|roots|fib_mod|gcd_lcm) \S+$", r"^python3 test_calc\.py$"],
            # the panel's calls arrive as `user`; an exec leg is sealed until its rule opens it
            "cwd": ".", "inherit_env": ["*"], "callable_by": {"user": ["*"]}, "ingress_rule": "allow_all_ingress",
            "view": {"kind": "html", "bytes": panel_html(), "calls": ["start", "watch"]}}


def panel_html() -> str:
    rows = "".join(f"<div><button id=b_{fn}>{fn}</button> <code>{json.dumps(UI[fn], separators=(',', ':'))}</code>"
                   f" → <span id=out_{fn}></span></div>" for fn in FNS)
    inputs = json.dumps({fn: json.dumps(UI[fn], separators=(",", ":")) for fn in FNS})
    html = f"""<h3>calc</h3>{rows}<script>
const IN = {inputs}; const job = {{}};
fantastic.watch('calc_exec', ev => {{
  if (!ev || ev.type !== 'progress') return;
  for (const fn in job) if (job[fn] === ev.job_id) document.getElementById('out_' + fn).textContent = ev.line;
}});
for (const fn in IN) document.getElementById('b_' + fn).onclick = async () => {{
  document.getElementById('out_' + fn).textContent = '…';
  const r = await fantastic.send('calc_exec', {{type: 'start', command: 'python3', args: ['calc.py', fn, IN[fn]]}});
  job[fn] = r && r.job_id;
}};
</script>"""
    return html


def panel() -> dict:
    """The page side is only a VIEW of `calc_exec`: layout + backend_id — its body
    and its grant come from the host record."""
    return {"id": "calc", "handler_module": "agent_view.ts", "display_name": "calc", "backend_id": "calc_exec",
            "owns_backend": False, "x": 700, "y": 60, "width": 520, "height": 300}



async def ui_values(pg) -> dict:
    from _lib.dev_surface import frame_eval
    vals = await frame_eval(pg, "JSON.stringify(Object.fromEntries([...document.querySelectorAll('[id^=out_]')].map(e=>[e.id.slice(4),e.textContent])))")
    for v in vals:
        if isinstance(v, str) and v.startswith("{") and len(json.loads(v)) == len(FNS):
            return json.loads(v)
    return {}


def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(rig.plug.record())
    rig.init(cells=[panel()], host_agents=[(FOLDER, exec_record())])

    async def drive():
        from _lib.dev_surface import click_button
        async with rig.page(1900, 1100) as pg:
            done, said = await ask(pg, 250, 300, ASK, rig.budget(rig.limits["total_seconds"], reserve=60), rig.limits["quiet_seconds"])
            await asyncio.sleep(2)
            clicked = [await click_button(pg, fn) for fn in FNS]
            got: dict = {}
            for _ in range(30):  # every output settled (each is one short process)
                await asyncio.sleep(0.5)
                got = await ui_values(pg)
                if got and all(v and v != "…" for v in got.values()):
                    break
            return done, said, clicked, got

    try:
        done, said, clicked, ui = asyncio.run(drive())
    finally:
        rig.stop()

    proj = rig.proj
    calc, tests = proj / "calc.py", proj / "test_calc.py"
    r: dict = {"calc_written": calc.exists(), "tests_written": tests.exists()}
    src = tests.read_text() if r["tests_written"] else ""
    r["tests_cover"] = all(fn in src for fn in FNS)
    rc, out = run(proj, "test_calc.py", timeout=120) if r["tests_written"] else (1, "")
    r["own_tests_pass"] = rc == 0
    hidden: dict = {}
    for fn in FNS:
        ok = r["calc_written"]
        for q in HIDDEN[fn] if ok else []:
            code, text = run(proj, "calc.py", fn, json.dumps(q, separators=(",", ":")))
            try:
                good = code == 0 and same(fn, last_json(text), REF[fn](q))
            except ValueError:
                good = False
            hidden.setdefault(fn, []).append(text.strip()[-200:])
            ok = ok and good
        r[f"hidden_{fn}"] = ok
    ui_ok = {}
    for fn in FNS:
        try:
            ui_ok[fn] = same(fn, json.loads(ui.get(fn, "")), REF[fn](UI[fn]))
        except ValueError:
            ui_ok[fn] = False
    r["ui"] = all(clicked) and all(ui_ok.values())
    rig.summary.update({"answer": said, "turn_quiet": done, "checks": r, "clicked": clicked, "ui": ui, "ui_ok": ui_ok,
                        "own_tests_out": out[-1500:], "hidden_out": hidden})
    print(f"ui: {ui}\nown tests rc={rc}")
    return verdict(*rig.finish(all(r.values()), " ".join(f"{k}={'✓' if v else '✗'}" for k, v in r.items())))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
