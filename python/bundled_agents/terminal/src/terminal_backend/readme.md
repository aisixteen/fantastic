# terminal_backend — PTY shell
Verb: shell (`cmd`) runs a command in a persistent PTY, streams token/done. Per-agent interrupt/stop. One persistent PTY **session per agent** — a session is exclusive (it carries one process + its scrollback; it is not shared or multiplexed), so a client owns the backend it attaches to. The process running in the PTY (e.g. `claude`) reaches the kernel via `fantastic` CLI one-shots or the web surface.

Streaming output is flow-controlled (VSCode's terminal model, ported): the PTY reader detaches once >100K emitted chars sit unacked and re-attaches once a consumer's `ack` verb drains the backlog — real backpressure so a flood can't lock up a tab. `write` is looped + per-agent serialized so large pastes land whole and bracketed-paste sequences can't interleave. `shell` is exempt (drains via scrollback).

`paste_image` (data:bytes, mime?) saves a pasted image to a per-agent scratch file and types its path into the PTY — bridges image paste for a CLI like `claude` running under the PTY (the backend can't reach a client's clipboard; path injection mimics a file drag-drop). png/jpeg/gif/webp, ≤5 MB.

PTY output is decoded with a per-session incremental UTF-8 decoder, so a multi-byte char split across an `os.read` chunk boundary is reassembled — no replacement-char (`<?>`) litter or column-shift line breaks on resize (node-pty's behaviour, ported).
