# E128 `export_tui` — the TUI's `/export <path>` command (#268)

The real `fantastic` TUI in a pseudo-terminal (the `screenshot` harness,
`src/tui/e2e/screenshot.rs`) over a project whose `.fantastic` holds the seeded
team (`lead` > `mid` > `leaf`, written the way the product stores agents, under `user`).

a) the tree lists `lead`; the keys select it; `/export out/tui` + Enter writes
   `out/tui` — a blueprint folder (nested, ports, no key / runtime field / row
   text); the composer is emptied and the screen shows no refusal (the TUI notes
   only refusals);
b) `/export ../x` shows "export: path may not hold `..`" and no `../x` folder exists;
c) `/export` with no path shows its usage and writes nothing.

No PTY (Windows): the test prints E2E-SKIP and ends.

Run: `cd src/lib/python && uv run python ../../../e2e/run.py --id export_tui` (scripted, free).
