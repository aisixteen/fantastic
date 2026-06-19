# python_runtime selftest

> scopes: kernel
> requires: `uv sync`; system Python on PATH
> out-of-scope: long-running REPL state, sandboxing

Async Python JOB spawner — `start` runs `python -u -c <code>` in the background,
returns a `job_id` at once, streams `progress`/`job_done` events; `status`/`stop`/
`interrupt` by job id. NOTE: a job lives only as long as the kernel process — a
one-shot `fantastic call` exits right after returning, so the CLI can only check
the verb SURFACE + error handling here. The full async lifecycle (start →
progress events → done/stop, parallel jobs) needs a LIVE kernel and is covered by
the in-process pytest suite (`tests/test_python_runtime.py`).

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
```

## Tests

### Test 1: reflect lists the job verbs + 0 running

```bash
PR=$(uv run --active fantastic kernel_state create_agent handler_module=python_runtime.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "PR=$PR"
uv run --active fantastic $PR reflect | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
need = {'start','status','stop','interrupt','clear','reflect','boot'}
ok = d['running'] == 0 and need <= set(d['verbs']) and 'job_done' in d['emits']
print('PASS' if ok else f'FAIL d={d}')
"
```

### Test 2: start returns a job_id (non-blocking)

```bash
uv run --active fantastic $PR start code='print(2*21)' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = d.get('status') == 'running' and bool(d.get('job_id'))
print('PASS' if ok else f'FAIL d={d}')
"
```
(The job's result + progress stream on a LIVE kernel — see pytest. A one-shot
exits before the pump runs.)

### Test 3: start rejects empty code

```bash
uv run --active fantastic $PR start code='' | grep -qF "code (str) required" && echo "PASS" || echo "FAIL"
```

### Test 4: unknown verb errors

```bash
uv run --active fantastic $PR garbage | grep -qF "unknown type" && echo "PASS" || echo "FAIL"
```

## Cleanup

```bash
rm -rf .fantastic
```

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | reflect lists start/status/stop/… + running=0 | |
| 2 | start returns a job_id (non-blocking) | |
| 3 | start rejects empty code | |
| 4 | unknown verb errors | |
| — | full async lifecycle (start→progress→done/stop, parallel) | pytest |
