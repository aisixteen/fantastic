# terminal_webapp — xterm UI

Browser xterm fronting a terminal_backend child (auto-created on boot). `get_webapp` → iframe descriptor; `upstream_id` tracks the backend.

Acks each output chunk back to the backend from xterm's parse callback (one `ack` per 5K chars) — the consumer half of VSCode-style flow control, so a flood can't outrun the renderer and lock the tab.

Catches image paste (which xterm drops — it only pastes text/plain) and ships the bytes to the backend's `paste_image`, so an image can be pasted into a CLI like `claude` running in the PTY.

## Web dependencies

xterm.js (v6.0.0) + fit addon (v0.11.0) are loaded from `/_assets/xterm.min.js`, `/_assets/xterm.min.css`, and `/_assets/xterm-addon-fit.min.js`, served by the `fantastic-web` bundle from vendored copies. No CDN dependency at runtime. To update the pinned versions, see `rust/crates/bundles/fantastic-web/src/readme.md`.
