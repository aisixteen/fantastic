# E59 calc — can the model write good multi-function code, and test it?

**Seeded (no model):** a panel `calc` with six buttons — `det`, `factor`,
`simpson`, `roots`, `fib_mod`, `gcd_lcm` — each with a fixed input; a click runs
`python3 calc.py <fn> <json>` on the exec agent `calc_exec` (which may run
exactly that and `python3 test_calc.py`) and shows the last output line.

**Asked (one shot):** write `calc.py` (stdlib only; one JSON line out per call)
and `test_calc.py` (its own tests, every function, exit 0 when green).

| fn | input | output |
|---|---|---|
| det | integer matrix | exact integer determinant |
| factor | integer n ≥ 2 | `[[p, e], …]`, p ascending |
| simpson | `{f, a, b, n}` (f: python expr in x, math names) | composite Simpson, float |
| roots | coefficients, highest first | distinct real roots, ascending |
| fib_mod | `[n, m]` (n up to 10^18) | F(n) mod m |
| gcd_lcm | list of integers | `[gcd, lcm]` |

**Checks** (fast — the harness runs the code itself, no extra turn):
`calc_written` · `tests_written` · `tests_cover` (every fn named in the tests) ·
`own_tests_pass` (`python3 test_calc.py` exits 0) · `hidden_<fn>` ×6 (two hidden
vectors each: a 6×6 exact determinant, a 12-digit factorization, F(10^18) mod p …)
· `ui` (each of the six buttons clicked shows the right value).
