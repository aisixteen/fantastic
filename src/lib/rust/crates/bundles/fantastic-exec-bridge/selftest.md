> scopes: cli
> requires: a real python3 NAMED in the `pr` record's `interpreters` allowlist — there is no default and no PATH probing

# exec_bridge selftest — durable script agents

Your `code` is a SCRIPT, not a set of verb handlers. It is run top to bottom, once per
`start` (and again on `boot`). Defining a function does not register a verb —
`def run(payload): ...` and nothing else defines `run`, exits, and does no work at all.
Put the work at module level. If the tool must survive its first pass to receive later
sends, keep it running; only a LIVE job acks `{queued:true}`.

The loader lays the `code` down as `main.py` beside `agent.json`. `start` spawns in the
BACKGROUND and returns at once; `boot` revives it after a reboot. There is **no `exec`**
— any verb the bundle does not own is MAIL for the running program, so a blocking
run-and-wait verb could never reach a live job's `on_message`.

The interpreter must be NAMED — `create_agent … python=/path/to/python3` (or `venv=`, or
the `FANTASTIC_PYTHON` env). Nothing is discovered off PATH, so an unconfigured agent
refuses `start` with `no Python interpreter resolved` instead of picking one for you.
ASK USER which python to use; do not guess an interpreter. And an explicitly named `venv`
that holds no interpreter is an ERROR naming the path — never a quiet downgrade to
another python, which would surface as `ModuleNotFoundError` for the packages you named
the venv for.

### Test 1: the verb surface

```sh
$FANTASTIC pr reflect | jq -e '[.verbs | keys[]] | contains(["start","status","clear","interrupt","stop","boot","reflect"])'
```

Expect: `true`. Also expect NO `exec` key — its presence is a regression.

### Test 2: start returns at once and streams

```sh
$FANTASTIC pr start code="import time
for i in range(3):
    print(i); time.sleep(0.2)"
```

Expect: `{job_id, status:"running"}` back immediately (well under a second), then
`progress` events one per line, then `job_done` with `ok:true` and `exit_code:0`.

### Test 3: the program lands on disk as a file

```sh
cat .fantastic/agents/<pr-id>/main.py
jq -e 'has("code") | not' .fantastic/agents/<pr-id>/agent.json
```

Expect: `main.py` holds the program verbatim, and `agent.json` does NOT carry a `code`
key. Two sources of truth for one program is the failure this split removes.

### Test 4: the connector reaches another agent

```sh
$FANTASTIC pr start code="print(kernel.send('kernel', {'type':'reflect'})['id'])"
```

Expect: the kernel's root id in a `progress` line. This proves the control socket, the
frame codec and reply correlation in one shot.

### Test 5: per-call arguments

```sh
$FANTASTIC pr start code="print(kernel.args['n'] + 1)" args='{"n":41}'
```

Expect: `42`. Absent `args` must give `{}`, never `null`.

### Test 6: an unknown verb is mail, and its absence is loud

Start a durable tool that registers `kernel.on_message`, then send it any verb the
bundle does not own.

Expect: `{queued:true}`, and the program receives it EXACTLY once. With no running job,
expect a loud error naming the verb — never a silent drop.

### Test 7: the orphan gate

Start a job that writes its pid and sleeps forever, then `ps` and `cancel task_id=<id>`.

Expect: `{cancelled:true}`, and `kill -0 <pid>` fails with ESRCH — the process is gone
AND reaped (a zombie would still answer). Then close the kernel while a job is parked:
the child must exit on its own, because the connector self-reaps on control-socket EOF.

If unreachable or a python is missing → STOP and report. Never silently fake a skip.
