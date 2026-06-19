# file_bridge — filesystem edge

Verbs: `read(path)` · `write(path,content)` · `list(path?)` · `delete(path)` · `rename(old,new)` · `mkdir(path)` · `reflect`
**Streams** (the SOURCE/SINK protocol — file_bridge is the fs implementor):
- `read_stream(path, offset=0, length=65536)` → `{bytes, next_offset, eof, size}` — `bytes` is the RAW chunk (rides the transport as raw bytes — web_ws / bridge binary frame, never base64); pull chunks until `eof` (stateless cursor).
- `write_stream(path, bytes, offset?, truncate?)` → push RAW chunks (`bytes` = one raw chunk; append by default; `truncate:true` on the first to start fresh). Text files use the whole-file `write`.
- `pump(source?, source_path, sink?, sink_path?, chunk=65536)` → server-side SOURCE→SINK copy in ONE call (vs driving each chunk over the wire). Both ends bound by id + the stream verbs, each SELF-gating — so any provider (a `network_bridge` SOURCE, a `file_bridge` SINK) composes. Omitted `source`/`sink` = this bridge.
A consumer (file serving · a pump · kernel_state) is **storage-agnostic** — it pulls/pushes by id, so a `network_bridge` answering the same verbs serves a *remote* file the same way (G1).

**Sealed by default** — open: `update_agent <id> ingress_rule=allow_all`.
**Running-dir law**: root is clamped inside the kernel's cwd — `../`, `~`, absolute paths
outside, and outward symlinks all refuse. Even when open, the edge never leaves the dir.
