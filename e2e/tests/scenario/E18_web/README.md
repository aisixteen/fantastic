# E18 · web — one ViewModel, drawn by the browser and the terminal

Free, CPU only. The runner (`e2e/runners/web.py`) is the page: it fetches it,
opens `/web_ui/ws` as the host's own origin, watches `ui`, sends intents and
rebuilds the screen from the frames the ViewModel publishes. Run 4 adds the
terminal View through the PTY rig on the same directory, at the same time.

    cd src/lib/python && uv run python ../../../e2e/run.py --only-free --id web

| run | proves |
|---|---|
| `first-web` | `fantastic --headless web --port N` inits, announces the URL, serves the page; `ui` says the browser sizes it; the frame shows the tree; a `v` key flips the sidebar to flat; SIGTERM releases the lock |
| `same-port-again` | the recorded `--port` again is accepted |
| `port-is-creation-only` | a DIFFERENT `--port` on an existing leg is refused, names it, exits non-zero |
| `rehydrate` | no flag: the port comes from the record; the frame shows the tree |
| `tui-and-web` | `fantastic web` (terminal is the default): `ui` says the terminal sizes it; a key in the browser appears in the terminal's frame, a key in the terminal appears in the browser's; clean exit, lock released |
| `last-recipe-shows` | `fantastic web ai`: both run, the LAST shows (the set-up page: no ai agent in this build) |
| `headless-stream` | `fantastic --headless`: the kernel's message stream on stdout, no escape codes, lock released |
