# `reboot_continuity` (E110) — the tree survives a reboot

Free: no model. The packaged host of `RUNTIME`, every call over `web_rest`:

1. builds a three-level chain (`top` → `mid` → `leaf`) with extra record fields
   of every JSON type and a `can` line on the caller (`top` → `ghost`; the target
   keeps no list), a `files` file_bridge with
   sixteen text files, a nested one and a binary one (raw bytes, octet leg), an
   `update_agent`, a deleted agent and a deleted file;
2. reboots one AGENT in process (delete + create on the same id, what
   `coding/E75_reboot_continuity` does to its AI agent): the memory store keeps
   the agent's planted rows, a re-created file_bridge lists every file;
3. stops the kernel (SIGTERM) and boots the same directory: every id under its
   parent, every field, the `can` line, the update, both deletes, the guid (read
   from the state folder; `rest`'s root reflect carries none), every file byte for byte, the agents answer, no execution id repeats;
4. does it once more with nothing changed in between.

The live E75 failure ("0 rows, 0/16 files listed" on rust after its reboot) is
checked at step 2 without a model.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id reboot_continuity --lanes 3
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E110_reboot_continuity/probe.py
```
