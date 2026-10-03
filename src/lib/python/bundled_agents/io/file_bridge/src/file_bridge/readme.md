# file_bridge — filesystem edge

Verbs: `read(path,offset?,limit?)` · `write(path,bytes,if_match?)` · `edit(edits,bytes,dry_run?)` · `append(path,bytes)` · `undo(path)` · `read_stream(path,offset?,length?)` · `write_stream(path,bytes,offset?,truncate?)` · `pump(source_path,sink?,sink_path?)` · `list(path?)` · `delete(path)` · `rename(old,new)` · `mkdir(path)` · `reflect`
**Streams** (the SOURCE/SINK protocol — file_bridge is the fs implementor):
- `read_stream(path, offset=0, length=65536)` → `{bytes, next_offset, eof, size}` — `bytes` is the RAW chunk (rides the transport as raw bytes — web_ws / bridge binary frame, never base64); pull chunks until `eof` (stateless cursor).
- `write_stream(path, bytes, offset?, truncate?)` → push RAW chunks (`bytes` = one raw chunk; append by default; `truncate:true` on the first to start fresh). Text files use the whole-file `write`.
- `pump(source?, source_path, sink?, sink_path?, chunk=65536)` → server-side SOURCE→SINK copy in ONE call (vs driving each chunk over the wire). Both ends bound by id + the stream verbs, each SELF-gating — so any provider (a `network_bridge` SOURCE, a `file_bridge` SINK) composes. Omitted `source`/`sink` = this bridge. **Both ends run AS THIS BRIDGE**: its `can_call` must reach the source (`read_stream`) and the sink (`write_stream`), and each end's `callable_by` must accept it.
**Text documents** (any format — the bridge imposes none):
- `read(path, offset=0, limit=200)` → a line-numbered page `{lines:[{n,text}], total, eof, version}`; refuses binary.
- `write(path, bytes, if_match?)` → atomic whole-document write; `append(path, bytes)` → one chunk at the end.
- `edit(edits:[{path, hunks, if_match?}], bytes)` → SEARCH/REPLACE hunks (`reflect` shows the grammar), a TRANSACTION over every listed file: any failure writes nothing.
- `version` = the document's hash; `if_match` refuses a write when the document changed since you read it.
- `undo(path)` → swaps the document with what the last write/edit/append replaced (a second `undo` is redo). ONE slot per path, in RAM — lost on restart, nothing kept on disk.

A consumer (file serving · a pump · kernel_state) is **storage-agnostic** — it pulls/pushes by id, so a `network_bridge` answering the same verbs serves a *remote* file the same way (G1).

**Deny-all at the path level** — the `allow` list on the record names what this bridge EXPOSES, relative to `root`: a file, or everything under a directory (`.` = the whole root). No list, or an empty one, exposes NOTHING. Every path verb sees only exposed paths; anything else is `not_found` whether or not it exists (nothing leaks past the list), and `list` also shows the directories on the way to an exposed path. BOTH ends count: the path as asked AND the file it lands on (symlinks followed) must be inside the list — a link that stays inside serves, a link pointing out of it serves nothing. Set it at create (`allow=["docs/a.pdf", "img"]`) or by `update_agent` (it replaces the list). Writes are still governed by `readonly`. Writing an exposed path creates its missing parent directories, even ones the list does not name (`allow:["a/b/c.txt"]` + `write a/b/c.txt` creates `a/` and `a/b/`); `list` shows them as the way to it, nothing else in them is exposed.

**Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`.
**Running-dir law**: root is clamped inside the kernel's running dir (the dir holding its root's `.fantastic`) — `../`, `~`, absolute paths
outside, and outward symlinks all refuse. Even when open, the edge never leaves the dir.
