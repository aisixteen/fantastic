# exec_bridge selftest

> scopes: kernel
> requires: `uv sync`; a real interpreter path to allowlist
> out-of-scope: HTTP, AI, sandboxing (there is none — see Test 1's note)

The process edge of the io family. Sealed by default, a regex command allowlist, the
control wire, mail to a live program, task-table integration.

Note on the binary: the console script is **`fantastic_kernel`**
(`pyproject.toml [project.scripts]`). Other selftests in this tree still write
`fantastic`, which does not exist — those are stale, this file is not.

## Pre-flight

All state under `/tmp/xb_test/`. `PY` is the interpreter the leg will allowlist —
the OPERATOR's choice, which is what the allowlist models. A program is a FILE; we
write it with a `file_bridge` leg and run it as a command.

```bash
rm -rf /tmp/xb_test && mkdir -p /tmp/xb_test && cd /tmp/xb_test
PY=$(python3 -c 'import sys; print(sys.executable)')
K=fantastic_kernel
# The store first, or one-shot seeds stay in RAM and the next call sees nothing.
$K kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools id=store root=. ingress_rule=allow_all_ingress >/dev/null
```

## Tests

### Test 0: sealed by default — every verb but `reflect` denies

```bash
SEALED=$($K kernel create_agent parent_id=kernel_state handler_module=exec_bridge.tools id=sealed | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
$K $SEALED start command=$PY args='["-c","print(1)"]'
$K $SEALED reflect | python3 -m json.tool | grep -F '"sealed": true'
```

Expect `start` → `{"error": ..., "reason": "unauthorized", "hint": ...}` naming
`ingress_rule=allow_all_ingress`, and `reflect` to answer anyway — discovery must work on a
sealed leg. Expect `"ingress_rule": "deny_ingress"`.

### Test 1: the allowlist is law, and the refusal names the legal set

```bash
$K kernel create_agent parent_id=kernel_state handler_module=exec_bridge.tools id=noalw ingress_rule=allow_all_ingress >/dev/null
$K noalw start command=$PY args='["-c","print(1)"]'   # empty allowlist → nothing runs
$K kernel update_agent id=noalw allowed='["/nope/python3 .*"]' >/dev/null
$K noalw start command=$PY args='["-c","print(1)"]'   # line does not match the pattern
```

Expect the first to refuse with "no commands are allowed on this leg ... an operator must
set `allowed=[...]`", and the second to refuse naming the `allowed` patterns it did not
match. A refusal that does not say what would work is a regression.

NOT a sandbox: an allowlisted CPython still has `open()`, `socket`, `subprocess`. The
allowlist fixes WHICH commands may run; it grants nothing else.

### Test 2: `start` → `status` → `stop`, and the job is a task-table row

```bash
XB=$($K kernel create_agent parent_id=kernel_state handler_module=exec_bridge.tools id=xb ingress_rule=allow_all_ingress allowed="[\"$PY .*\"]" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
JOB=$($K xb start command=$PY args='["-c","import time; print(\"up\", flush=True); time.sleep(30)"]' | python3 -c 'import json,sys;print(json.load(sys.stdin).get("job_id",""))')
$K xb status job_id=$JOB | python3 -m json.tool | grep -E '"(status|task_id|last_line)"'
$K kernel ps | grep -F '"verb": "start"'
$K xb stop | grep -F '"killed": 1'
```

Expect `status` → `running`, a non-null `task_id`, `last_line: "up"`; `ps` to list the
row; `stop` to report one kill.

### Test 3: `cancel` reaps the real OS process

```bash
JOB2=$($K xb start command=$PY args='["-c","import time; time.sleep(60)"]' | python3 -c 'import json,sys;print(json.load(sys.stdin)["job_id"])')
TID=$($K xb status job_id=$JOB2 | python3 -c 'import json,sys;print(json.load(sys.stdin)["jobs"][0]["task_id"])')
$K kernel cancel task_id=$TID
$K xb status job_id=$JOB2 | grep -E '"status": "(stopped|failed)"'
```

Expect the row to finalize, NOT to stay `running`. The OS subprocess is SIGKILLed by
`on_cancel` before the kernel aborts the pump task — the orphan-bug regression.

### Test 4: a program reaches another agent over the control wire

```bash
$K kernel create_agent parent_id=kernel_state handler_module=durable_memory.tools id=mem file_bridge_id=store callable_by='{"xb":"*"}' >/dev/null
$K kernel update_agent id=xb can_call='{"mem":"*"}' >/dev/null
# Write the program to a FILE (connector + body), then run it as a command.
python3 - <<'PYGEN' >/dev/null
from exec_bridge._connector import CONNECTOR_SRC
open("prog.py","w").write(CONNECTOR_SRC + 'kernel.send("mem", {"type":"set","key":"k","value":"via-wire"})\n')
PYGEN
$K xb start command=$PY args='["prog.py"]'
sleep 1; $K mem read key=k | grep -F 'via-wire'
$K xb reflect | python3 -m json.tool | grep -E '"(allowed|wire)"'
```

Expect the value to land, and `reflect` to advertise the `allowed` gate + the `wire`
one-liner. The program includes the reference wire client itself — the bridge prepends
nothing.

### Test 5: mail to a live program, and the loud error with none

```bash
python3 - <<'PYGEN' >/dev/null
from exec_bridge._connector import CONNECTOR_SRC
open("mailer.py","w").write(CONNECTOR_SRC + 'import threading\nkernel.on_message(lambda m: kernel.emit({"type":"say","text":"got"}))\nthreading.Event().wait()\n')
PYGEN
$K xb start command=$PY args='["mailer.py"]'
sleep 1
$K xb anything_at_all | grep -F '"queued": true'
$K xb stop >/dev/null
$K xb anything_at_all | grep -F 'no running job to receive it'
```

Expect `{queued:true}` while a job is live and a loud error once none is. The bundle
performs no delivery — the substrate's dispatch fanout already mirrored the payload into
the program's proxy inbox; the handler only ACKs.

### Test 6: a durable program re-runs against fresh input

```bash
echo 100 > a.txt; echo 200 > b.txt
python3 - <<'PYGEN' >/dev/null
from exec_bridge._connector import CONNECTOR_SRC
open("total.py","w").write(CONNECTOR_SRC + 'import os\nt=sum(int(open(f).read()) for f in os.listdir(".") if f.endswith(".txt"))\nkernel.send("mem", {"type":"set","key":"total","value":t})\n')
PYGEN
$K xb start command=$PY args='["total.py"]'
sleep 1; $K mem read key=total | grep -F '300'
echo 50 > c.txt                         # fresh input
$K xb start command=$PY args='["total.py"]'   # SAME command, re-run
sleep 1; $K mem read key=total | grep -F '350'
```

Expect the total to be recomputed from disk each run — 300, then 350. The program is a
durable FILE; re-running the same command is what "keeps it up to date". Probe 04's canary
depends on exactly this.

## Regression signals

- `start` on a sealed leg SUCCEEDING — the gate is bypassed.
- A refusal that does not name the legal `allowed` patterns.
- `start` accepting a `code`/`shim`/`interpreter`/`interpreters`/`argv_template` field
  instead of refusing it — the withdrawn recipe must stay refused.
- Two `say` events after restarting a durable tool — the `_owner_inbox` supersede
  failed and every inbound send is doubled.
- An `exec` key in `reflect.verbs` — that verb never existed here.
