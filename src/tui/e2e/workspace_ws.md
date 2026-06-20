# Scenario: @ws workspace kernel (spawn → drive → stop)

The manager spawns a sovereign `fantastic_kernel` PROCESS in the cwd and drives it
over HTTP, then stops it. This is the multi-runtime path; it needs the kernel
binary resolvable (FANTASTIC_KERNEL_BIN, or the dev build at
src/lib/rust/target/release/fantastic_kernel).

```script
wait 1500
key space
wait 500
type @ws up
key enter
# spawning + seeding the serve surface takes a couple seconds
wait 4000
shot ws_up
type @ws list_agents
key enter
wait 1500
shot ws_list
type @ws down
key enter
wait 1500
shot ws_down
```

## Expect
- `ws_up`: a note like `workspace spawned at http://127.0.0.1:<port>` and the
  header chip changes from `ws: none` to `ws: 127.0.0.1:<port>`. (If the kernel
  binary isn't built, expect instead an error note `✗ ws: … image/bin not present`
  — that's a clean skip, not a crash.)
- `ws_list`: a reply listing the workspace kernel's agents (incl `core`, `web*`).
- `ws_down`: a note that the workspace stopped; chip back to `ws: none`.

## Pass / fail
PASS if up→list→down each render their note/reply (or `up` cleanly reports the
missing binary). FAIL on a crash or a hang.

## ⚠ State / cleanup — a FUNDAMENTAL question this scenario surfaces
`@ws up` writes a real `.fantastic/` (store + agents + lock.json + serve.log) into
the **cwd**. The harness runs in a throwaway temp cwd so the repo isn't polluted —
but for a real user this means "where does a workspace kernel's state live?" is a
DESIGN decision (cwd `.fantastic`, like git? a per-project dir? `~/.fantastic`?).
The brain's own history (`ai_fs`) ALSO currently roots at the cwd. Flag both for a
deliberate decision rather than letting cwd be the accidental default.
