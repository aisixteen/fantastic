# python_runtime selftest

> scopes: kernel
> requires: `uv sync`; system Python on PATH
> out-of-scope: long-running REPL state, sandboxing

Subprocess Python exec — `python -c <code>`, stateless per-call.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
```

## Tests

### Test 1: reflect lists verbs + 0 in_flight

```bash
PR=$(uv run --active python kernel.py call core create_agent handler_module=python_runtime.tools | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "PR=$PR"
uv run --active python kernel.py call $PR reflect | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = d['in_flight'] == 0 and 'exec' in d['verbs'] and 'interrupt' in d['verbs']
print('PASS' if ok else f'FAIL d={d}')
"
```

### Test 2: exec print

```bash
uv run --active python kernel.py call $PR exec code='print(2*21)' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = '42' in d['stdout'] and d['exit_code'] == 0 and not d['timed_out']
print('PASS' if ok else f'FAIL d={d}')
"
```

### Test 3: exec captures stderr + non-zero exit

```bash
uv run --active python kernel.py call $PR exec code='import sys;sys.stderr.write("oops");sys.exit(7)' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = 'oops' in d['stderr'] and d['exit_code'] == 7
print('PASS' if ok else f'FAIL d={d}')
"
```

### Test 4: exec timeout fires fast

```bash
START=$(python -c "import time;print(time.time())")
uv run --active python kernel.py call $PR exec code='import time;time.sleep(60)' timeout=0.4 | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
print('PASS' if d['timed_out'] and d['exit_code'] != 0 else f'FAIL d={d}')
"
END=$(python -c "import time;print(time.time())")
ELAPSED=$(python -c "print(f'{$END-$START:.2f}')")
echo "  elapsed: ${ELAPSED}s"
```
Expected: `PASS` and elapsed < 3s.

### Test 5: exec respects cwd from agent record

```bash
mkdir -p /tmp/pr_cwd
uv run --active python kernel.py call core update_agent id=$PR cwd=/tmp/pr_cwd >/dev/null
uv run --active python kernel.py call $PR exec code='import os;print(os.getcwd())' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
print('PASS' if '/tmp/pr_cwd' in d['stdout'] else f'FAIL stdout={d.get(\"stdout\")!r}')
"
rmdir /tmp/pr_cwd
```

### Test 6: exec rejects empty code

```bash
uv run --active python kernel.py call $PR exec code='' | grep -qF "code (str) required" && echo "PASS" || echo "FAIL"
```

### Test 7: unknown verb errors

```bash
uv run --active python kernel.py call $PR garbage | grep -qF "unknown type" && echo "PASS" || echo "FAIL"
```

## Cleanup

```bash
rm -rf .fantastic
```

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | reflect lists verbs + in_flight=0 | |
| 2 | exec print → stdout 42 | |
| 3 | exec captures stderr + exit_code | |
| 4 | exec timeout fires <3s | |
| 5 | exec respects record cwd | |
| 6 | exec rejects empty code | |
| 7 | unknown verb errors | |
