# E127 `export_cell_menu` — the cell menu's "Export…" writes a blueprint folder (#268)

A real host and a real page. The seeded team (`lead` > `mid` > `leaf`) is shown by
one cell (`cell_e127`, `backend_id: lead`); a second cell (`cell_e127_page`) has
no host agent. The page's `prompt` is replaced (a native dialog is not CDP
friendly) and answers `out/menu`.

a) a right-click (a CDP mouse event, button right) on the cell's head opens the
   menu: an "Export…" item is among its buttons;
b) a click on it asks the host `user export_subtree` (the host's own stream shows
   the call), the page toasts "exported 3 agents to out/menu", and `out/menu`
   holds the blueprint: nested, ports, no key / runtime field / row text;
c) the page-only cell's menu "Export…" toasts "has no host agent", asks the host
   nothing more and writes no folder;
d) a prompt answered with nothing writes nothing; no page error is logged.

Run: `cd src/lib/python && uv run python ../../../e2e/run.py --id export_cell_menu` (scripted, free).
