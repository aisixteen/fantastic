# `exec_jobs` (E115) — exec_bridge jobs over REST

Free: no model. The kernel's own `exec_bridge` on the packaged host of `RUNTIME`,
every call over `web_rest`, the agent's events (`job_done`) watched over the `ws` face.

What it proves (details in `probe.py`'s docstring):

- an allow-listed line runs as a job; an unmatched line is refused `unauthorized` and
  starts nothing; a record (seeded on disk) whose only pattern is a broken regex
  permits nothing; `update_agent` refuses a broken regex and leaves the list as it was;
- `job_done` is exactly `{type, job_id, exit_code, ok, lines, last_line}` — no
  stdout/stderr — for a clean and a failing exit; a PTY job's is `{type, job_id, exit_code, ok}`;
- `tail`/`grep` over a job that printed 20000 lines (~660 KB): the ring keeps the newest
  256 KiB (line bytes + 1 each), `dropped`/`printed`/`scanned` count exactly, the oldest
  kept line is found and the newest dropped one is not; `stream` filters stdout/stderr;
- a PTY job: `write` reaches it, `tail`/`output` read it, `status` carries cols/rows;
- kernel `cancel task_id=<job_id>`: `ps` listed the job, the OS process dies, the row
  settles `stopped` and leaves `ps`;
- `status` shape for one job (with `cwd`) and for the summary (without); a string
  `job_id` is `bad_arg`, an unknown one `not_found`.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id exec_jobs --lanes 3   # all three
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E115_exec_jobs/probe.py
```
