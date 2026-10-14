# `tui_wizard` — the terminal's AI set-up wizard, keyboard-driven in a PTY

The canvas's two-page wizard, in the terminal: a scripted
keyboard in a pseudo-terminal (`src/tui/e2e/screenshot.rs`), the vLLM backend
linked, **no model called** — an agent is made, never asked.

1. **create** — page 1 (nothing saved, the four providers, only vllm linked) →
   vllm → page 2 (`vllm-1`, the URL filled) → a model, a pasted key kept plain
   (masked), kv_overflow → CREATE & OPEN CHAT: the saved set-up (the default),
   the key, the agent, its chat open.
2. **from saved** — a new launch: enter on `vllm-1 ★ default` makes `vllm-1-2`.
3. **edit + default** — `e`: EDIT SAVED (the stored key offered for its host), a
   new model saved back; `d`: no default.

    cd src/lib/python && uv run python ../../../e2e/runners/tui.py tui_wizard
