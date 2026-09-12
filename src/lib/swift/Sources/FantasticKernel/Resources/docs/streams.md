> Chunked byte streams (files, feeds) — a PULL with cursor + backpressure, raw bytes.

# Streams — bytes, not events

`send` returns ONE JSON reply, and `emit` fires ONE event. For BYTES that
don't fit a single message — a file, an image, a live feed — agents speak a
chunked **stream** protocol over the `binary` transport (raw bytes both
ways, never base64). A stream is NOT the event system: it is a PULL with a
cursor and backpressure, addressed by agent id. Three duck-typed verbs — any
agent that answers them is a stream end:

- `read_stream {path, offset?}` → `({path, offset, next_offset, eof, size}, bytes)`
  — the **SOURCE**. Pull ONE chunk; the bytes ride the reply BODY. Stateless
  cursor (no open handle): get the next chunk by calling again with
  `offset=next_offset`, until `eof`.
- `write_stream {path, offset?, truncate?}` + body `bytes` → `{path, written, offset, size}`
  — the **SINK**. Push ONE chunk at `offset` (default: append); `truncate=true`
  on the first chunk starts fresh.
- `pump {source, source_path, sink, sink_path?, chunk?}` → `{source, sink, bytes, chunks}`
  — the **PUMP**: a server-side SOURCE→SINK copy, chunk by chunk, in ONE call.
  It only coordinates (drives `read_stream` → `write_stream` by id) and never
  touches the bytes, so it copies fs→fs, `network_bridge`→`file_bridge`,
  anywhere→anywhere identically. Each end self-gates + self-clamps; a sealed
  end refuses.

A consumer is therefore storage-agnostic — it pulls/pushes by id, blind to
what's behind it (a file, a socket, a generator). `file_bridge` is the
reference SOURCE+SINK; the served `GET /<rest>/file/<path>` octet route is
read_stream-only.

## The kernel's tail

`Kernel.rows_stream(after)` is a SOURCE inside the process: every message with
`id > after`, then every message as it lands. Yielding is not a message — no send, no
task, no row. The loader's message pump is its first consumer; a UI or a relay can be
the next, at no cost to the agents.
