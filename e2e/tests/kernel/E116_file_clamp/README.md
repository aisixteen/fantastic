# `file_clamp` (E116) — file_bridge's clamp and allow list over REST

Free: no model. The kernel's own `file_bridge` (root `box/`, `allow: [pub, bin]`) on
the packaged host of `RUNTIME`, every call over `web_rest`, its `changed` events watched
over the `ws` face.

What it proves (details in `probe.py`'s docstring):

- `..` out of the root, `..` to an unexposed sibling, and absolute paths are refused
  for read, read_stream and write, and no refused reply leaks the file's content;
- a symlink inside `pub` pointing out of the root (and the running dir), or to an
  unexposed file inside the root, serves nothing; `list` hides it; a link that stays
  in the exposed area serves;
- write / append / write_stream through such a link are refused and the target is
  untouched — also when a regular file the bridge wrote is swapped for the link;
- repeated atomic writes leave no temp file; a write creates its parent dirs;
- `allow` is read live: narrowed by `update_agent`, the next read/write obeys; widened,
  the next call serves; an entry climbing out of the root is refused at the write;
- `watch_path` refuses an unexposed path; a watched dir dropped from `allow` reports
  nothing more while a still-exposed watched dir keeps reporting;
- `write_stream`/`read_stream` carry raw bytes (all 256 values, 3 × 64 KiB chunks), the
  disk holds exactly them; `read` refuses a binary file.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id file_clamp --lanes 3   # all three
```
