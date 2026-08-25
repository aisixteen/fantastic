# exec_bridge selftest

> scopes: kernel
> requires: `uv sync`; a real interpreter path to allowlist
> out-of-scope: HTTP, AI, sandboxing (there is none — see Test 1's note)

The process edge of the io family. Sealed by default, interpreter allowlist, the
control wire, the optional shim, mail to a live program, task-table integration.

Note on the binary: the console script is **`fantastic_kernel`**
(`pyproject.toml [project.scripts]`). Other selftests in this tree still write
`fantastic`, which does not exist — those are stale, this file is not.

## Pre-flight

All state under `/tmp/xb_test/`. `PY` is the interpreter the leg will allowlist —
the OPERATOR's choice, which is what the allowlist models. Nothing is written to
the project tree.

```bash
rm -rf /tmp/xb_test && mkdir -p /tmp/xb_test && cd /tmp/xb_test
PY=$(python3 -c 'import sys; print(sys.executable)')
K=fantastic_kernel
# The store first, or one-shot seeds stay in RAM and the next call sees nothing.
$K kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools id=store root=.fantastic ingress_rule=allow_all_ingress >/dev/null
```

## Tests

### Test 0: sealed by default — every verb but `reflect` denies

```bash
SEALED=$($K kernel create_agent parent_id=kernel_state handler_module=exec_bridge.tools id=sealed | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
$K $SEALED start code='print(1)'
$K $SEALED reflect | python3 -m json.tool | grep -F '"sealed": true'
```

Expect `start` → `{"error": ..., "reason": "unauthorized", "hint": ...}` naming
`ingress_rule=allow_all_ingress`, and `reflect` to answer anyway — discovery must work on a
sealed leg. Expect `"ingress_rule": "deny_ingress"`.

### Test 1: the allowlist is law, and the refusal names the legal set

```bash
$K kernel create_agent parent_id=kernel_state handler_module=exec_bridge.tools id=noalw ingress_rule=allow_all_ingress >/dev/null
$K noalw start code='print(1)'                      # empty allowlist
$K kernel update_agent id=noalw interpreter=/nope/python3   # not allowlisted
```

Expect the first to refuse with "no interpreters are allowlisted ... an operator must
set `interpreters=[...]`", and the second to be refused **at the write** naming the
allowed set. A refusal that does not say what would work is a regression.

NOT a sandbox: an allowlisted CPython still has `open()`, `socket`, `subprocess`. The
allowlist fixes WHICH interpreter and environment; it grants nothing else.

### Test 2: `start` → `status` → `stop`, and the job is a task-table row

```bash
XB=$($K kernel create_agent parent_id=kernel_state handler_module=exec_bridge.tools id=xb ingress_rule=allow_all_ingress interpreters="[\"$PY\"]" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
JOB=$($K xb start code='import time
print("up", flush=True)
time.sleep 30' 2>/dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin).get("job_id",""))')
$K xb status job_id=$JOB | python3 -m json.tool | grep -E '"(status|task_id|last_line)"'
$K kernel ps | grep -F '"verb": "start"'
$K xb stop | grep -F '"killed": 1'
```

Expect `status` → `running`, a non-null `task_id`, `last_line: "up"`; `ps` to list the
row; `stop` to report one kill. Substitute a valid `time.sleep(30)` — the line above is
deliberately malformed to check the failure path reports `exit_code != 0` with the
`SyntaxError` on stderr.

### Test 3: `cancel` reaps the real OS process

```bash
JOB2=$($K xb start code='import time; time.sleep(60)' | python3 -c 'import json,sys;print(json.load(sys.stdin)["job_id"])')
TID=$($K xb status job_id=$JOB2 | python3 -c 'import json,sys;print(json.load(sys.stdin)["jobs"][0]["task_id"])')
$K kernel cancel task_id=$TID
$K xb status job_id=$JOB2 | grep -E '"status": "(stopped|failed)"'
```

Expect the row to finalize, NOT to stay `running`. The OS subprocess is SIGKILLed by
`on_cancel` before the kernel aborts the pump task — the orphan-bug regression.

### Test 4: the wire vs the shim

```bash
$K kernel create_agent parent_id=kernel_state handler_module=yaml_state.tools id=mem file_bridge_id=store >/dev/null
# With the shim: `kernel` is in scope.
$K xb start shim=python code='kernel.send("mem", {"type":"set","key":"k","value":"via-shim"})'
sleep 1; $K mem read key=k | grep -F 'via-shim'
# Without: the program frames it itself. `reflect readme=true` on the leg for the wire.
$K xb reflect | python3 -m json.tool | grep -E '"(shims|interpreters)"'
```

Expect the shim path to land the value, and `reflect` to advertise both the served
shims and the allowlist. A program naming an unknown shim is refused, naming the set.

### Test 5: mail to a live program, and the loud error with none

```bash
$K xb start shim=python code='import threading
kernel.on_message(lambda m: kernel.emit({"type":"say","text":"got"}))
threading.Event().wait()'
sleep 1
$K xb anything_at_all | grep -F '"queued": true'
$K xb stop >/dev/null
$K xb anything_at_all | grep -F 'no running job to receive it'
```

Expect `{queued:true}` while a job is live and a loud error once none is. The bundle
performs no delivery — the substrate's dispatch fanout already mirrored the payload
into the program's proxy inbox; the handler only ACKs.

### Test 6: the record's `code` is durable

```bash
$K kernel update_agent id=xb code='print("FROM_THE_RECORD")' >/dev/null
$K xb start                       # no code in the payload
sleep 1; $K xb status | grep -F 'FROM_THE_RECORD'
```

Expect `start` with no `code` to run the record's. Probe 04's canary and probe 05's
clock payload both depend on this; breaking it breaks the emergence set.

## Regression signals

- `start` on a sealed leg SUCCEEDING — the gate is bypassed.
- A refusal that does not name the legal interpreters.
- `boot` returning `None` for a record whose interpreter is not allowlisted (the
  silent-success hole this bundle exists to close; unit-tested in
  `tests/test_exec_bridge_jobs.py`).
- Two `say` events after restarting a durable tool — the `_owner_inbox` supersede
  failed and every inbound send is doubled.
- An `exec` key in `reflect.verbs` — that verb never existed here.
