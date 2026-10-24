# `file_agents` (E123) — file and folder agents over a file_bridge

Free: no model. The kernel's own `file_bridge` (root `box/`, `allow: [docs, top.txt]`) on
the packaged host of `RUNTIME`, every call over `web_rest`, the live events over the `ws`
face. A file or a folder is a VIEW AGENT of the bridge, made by `open_view` under an id
the harness chooses and declares at boot (so the ws leg may watch it).

What it proves (details in `probe.py`'s docstring):

- `open_view` makes a file view and a folder view under their chosen ids, and a second
  open of the same path is the same agent; a chosen id that is another view's is refused;
  an unexposed path, a `..`, an absolute path or `.` as a file makes none;
- the file view reads its file, writes against the version it read (a stale version is
  refused and the disk is untouched), and names no other path (nothing of it leaks);
- the folder view lists inside its folder, refuses a climb out of it, reveals the way
  down, and says `not_found` for what is not there;
- `watch` arms a view's relay once: a second `watch` of the same view is the same relay;
- live, over the `ws` face: a file view emits `changed` after an on-disk edit, and a
  folder view emits `added` for an entry made in it and `removed` for that entry deleted.

The canvas drop and the TUI views are the page's, unit-tested in
`src/tui/web/canvas/tests/file_agents.test.ts` and `src/tui/src/panel/files.rs`; this
probe pins the kernel contract they stand on.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id file_agents --lanes 3   # all three
RUNTIME=swift uv run python ../../../e2e/tests/kernel/E123_file_agents/probe.py
```
