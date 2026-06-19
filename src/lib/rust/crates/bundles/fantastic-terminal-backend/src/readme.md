# terminal_backend — PTY shell session as an agent

One PTY per agent. Process-memory state only; no `.fantastic/` sidecars
(paste blobs go to the OS tempdir). Verbs (verb-for-verb mirror of the
Python `terminal_backend.tools`):

- `reflect`     → `{id, sentence, cmd, cwd, env, cols, rows, running, in_flight_bytes, unacked, paste_dir?, verbs, emits}`
- `boot`        — if `record.auto_start` (default true), spawns the PTY via `record.cmd` (default `["bash"]` on Unix, `["powershell"]` on Windows). Idempotent.
- `spawn`       — `{cmd:[str], cwd?, env?, cols?, rows?}` start PTY child. Overrides record meta if payload args present.
- `write`       — `{data:str}` write UTF-8 bytes to PTY master. Serialized per-agent.
- `ack`         — `{count:int}` decrement unacked-byte counter; resumes reader if it falls below 100K.
- `resize`      — `{cols:int, rows:int}` fires SIGWINCH; TUI apps redraw.
- `paste_image` — `{data:bytes|base64, mime?}` save bytes to OS tempdir; cap 5 MB; type absolute path + trailing space into PTY.
- `interrupt`   — SIGINT to PTY child.
- `signal`      — `{signal:str|int}` send named signal.
- `stop`        — SIGKILL the child; close master; remove from TERMINALS map.

Streaming output is flow-controlled (VSCode terminal model, ported):
the PTY reader pauses once >100K emitted bytes sit unacked, resumes
once a consumer's `ack` verb drains the backlog below 100K. `write` is
looped and per-agent serialized so large pastes land whole and a
bracketed-paste sequence can't interleave with another write.

`paste_image` overrides `Bundle::handle_binary` for direct byte access:
a clipboard image arrives as a binary frame and skips the base64
round-trip; the bytes are written to a per-agent scratch file in the OS
tempdir and the absolute path is typed into the PTY with a trailing
space (mimics a drag-drop, doesn't submit). The backend can't reach a
client's clipboard, so path injection bridges the paste.

PTY output is decoded with a per-session incremental UTF-8 decoder
(`encoding_rs::Decoder`), so a multi-byte char split across a read
boundary is reassembled — no replacement-char (`<?>`) litter or column-
shift line breaks on resize. Mirrors what node-pty does for VSCode.

Events emitted to this agent's **OWN** inbox (a client watches it):

- `{type:"data", text:str}` — decoded output (per read chunk)
- `{type:"exited", exit_code:i32}` — child died
- `{type:"error", error:str}` — read/write failure
