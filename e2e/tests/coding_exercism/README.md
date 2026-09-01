# coding_exercism — the emergent coder's PASS-RATE (a benchmark, not a lane)

Runs the whole coder loop over Aider's polyglot-benchmark (Exercism) exercises
— each a born-failing mini-repo with its OWN test suite as the objective
oracle. Zero kernel code: the E43 trio (file_bridge + text_file editor +
exec_bridge runner) driven by the live model; scored by rerunning the
exercise's suite (+ never-touched-test check).

This is a BENCHMARK: heavy, paid, resumable — like `e2e/` emergence harness, it
is NOT in run.py's groups. Run it directly.

    cd e2e && set -a && . backends/connection.local.env && set +a
    python tests/coding_exercism/gym.py --runtime python --limit 5   # cheap smoke
    python tests/coding_exercism/gym.py --runtime python --only bowling
    python tests/coding_exercism/gym.py --runtime rust               # full python set on rust

Backend from `e2e/backends/` (env), exactly like the e2e. The clone at
`tmp/repos/polyglot-benchmark` stays pristine (per-exercise workdir, cleaned).
Prints a per-exercise table + a pass-rate per runtime.

Iter-1: python exercises, host (no container), 3 kernels. Roadmap: other 5
languages (→ container venue), a committed baseline to diff, per-model compare,
an E55 map-first arm. See tmp/exercism_gym.md.
