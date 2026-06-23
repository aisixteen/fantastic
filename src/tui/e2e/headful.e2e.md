# Headful TUI e2e — operator (LLM) guide

The TUI is interactive (raw mode, alternate screen, animation) so it can't be
asserted with ordinary unit tests. This suite drives the **real `fantastic`
binary inside a pseudo-terminal**, scripts keystrokes, and captures the rendered
**vt100 cell grid as text "screenshots"**. An operating agent (you) runs a
scenario, reads the frames, and judges PASS/FAIL against the scenario's `## Expect`.

Colors don't survive the text dump — but layout, text, and the block-font `█`
FANTASTIC title do, which is enough to verify legibility and behaviour headless.

## How it works
- **Harness**: `src/shared/term/examples/screenshot.rs` — a `cargo example` that
  opens a PTY, spawns the binary, runs a script, writes frames. Runs the binary in
  a **throwaway temp cwd** so `@sh`/`@ws`/brain-history never touch the repo.
- **Runner**: `run.sh` — extracts the ` ```script ` block from a scenario `.md`
  and feeds it to the harness.
- **Scenarios**: the `*.md` files here — each is a script + human/LLM-readable
  `## Expect` + `## Pass / fail`.

## Script verbs (inside a scenario's ```script block)
```
wait <ms>            sleep
type <text…>         send literal text (rest of the line)
key  <name>          space|enter|esc|tab|backspace|ctrl-c|ctrl-f|ctrl-q|up|down|left|right|<char>
shot <label>         capture one frame  → NN_<label>.txt
stream <ms> <count>  capture <count> frames every <ms>  → NN_stream.txt  (use for animations)
```
**Streaming is the smart strategy for anything animated** (the attract reveal, the
intro movie, live AI streaming): capture a frame every N ms and flip through them.

## Run a scenario
```sh
cd src/tui/e2e
sh run.sh attract.md                 # frames → /tmp/ft-e2e/attract/
sh run.sh chat_kernel.md /tmp/out     # custom out dir
FT_COLS=120 FT_ROWS=34 sh run.sh attract.md   # override PTY size
```
Then read every `NN_*.txt` frame in the out dir, compare to the scenario's
`## Expect`, and report PASS/FAIL per scenario with the deciding frame quoted.

## Scenarios
- `attract.md` — the arcade title card (legible FANTASTIC + starfield + prompt).
- `chat_kernel.md` — enter chat, `@kernel list_agents`, reply renders.
- `terminal_sh.md` — `@sh` breathing PTY viewport.
- `rooms.md` — per-character rooms: paste into `@ai`/`@kernel`/`@sh`, Shift-Tab
  between them; the `@sh` viewport stays in its room (no bleed). Needs a `shift-tab`
  key (CSI Z) in the harness.
- `intro_movie.md` — `/intro` plays the movie (streamed).
- `workspace_ws.md` — `@ws up`/`list`/`down` spawns + drives + stops a real kernel.

## Prereqs
- `cd src && cargo build` (the runner builds it if missing).
- `workspace_ws.md` additionally needs the kernel binary
  (`cargo build --release -p fantastic-cli` in `src/lib/rust`, or `FANTASTIC_KERNEL_BIN`);
  without it `@ws up` reports a clean error instead of spawning.

## When a run reveals something FUNDAMENTAL
These are headful, real-binary runs, so they expose real behaviour — notably
**where state is written**. If a scenario shows the binary persisting somewhere
that should be a deliberate choice (the brain's `ai_fs` history, a workspace
`.fantastic/`, the chat transcript), DON'T just record PASS — surface it as a
design question. Current known flag: the manager brain's `ai_fs` and `@ws`
workspaces both root at the **cwd**; "where does state live?" (cwd vs per-project
vs `~/.fantastic`) is an open architectural decision.

## Cleanup
The harness kills the child and removes its temp cwd. `@ws` scenarios should end
with `@ws down` to stop the spawned kernel. If a run is interrupted, check for a
stray `fantastic_kernel` process and a `/tmp/ft-e2e-run-*` dir.
