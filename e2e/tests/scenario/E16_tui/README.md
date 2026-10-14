# `tui` — the `fantastic` shell, keyboard-driven in a PTY

The shell binary driven by a **scripted keyboard** in a pseudo-terminal, using
the shell's own rig (`src/tui/e2e/screenshot.rs`), which renders the screen
through vt100 into text frames. No model.

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --only-free --id tui
uv run python ../../../e2e/runners/tui.py tui --bin /path/to/fantastic
```

| run | proves |
|---|---|
| `attract-leave` | the intro shows the block title, and leaving it without a key writes **nothing** — no `.fantastic` at all (the explicit init gate) |
| `init` | one key inits from `blueprint.yaml`: the live tree shows every blueprint agent and no `mcp`; Ctrl-C ×2 exits 0 with the lock released and records and the memory store `memory.db` on disk |
| `rehydrate` | the same dir skips the intro, shows the tree straight away, and the store's rows grow in the same `memory.db`; `mcp` opens in the default view |
| `activity` | the `✉ Activity` row under the kernel opens the page of open questions (0 open on the base tree) |
| `terminal` | `t` opens `$SHELL` below the page as the agent `term`; typed keys reach it in order (`hi-42` from the shell's own arithmetic); `t` closes it and its record is deleted |
| `sidebar` | `v` flat (`# name` channels, `+ 2 system`), `<` the rail of tiles, `>` back, `v` the tree again |
| `setup` | `+ add an ai agent` opens the four-field page; with no backend linked it says so, and WRITE THE RECORD is refused in place (`missing: id, handler, model`) |
| `mouse` | the rig sends a real SGR mouse click on a sidebar row; the row is selected and opened (panels are boxed) |

`--bin` / `FANTASTIC_BIN` runs any binary; otherwise the runner builds `src/tui`
(the binary and the rig).
