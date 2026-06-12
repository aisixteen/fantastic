# file_bridge selftest

> scopes: kernel, persistence
> requires: `uv sync`
> out-of-scope: HTTP, AI, real network filesystems

The fs edge of the io family. Sealed by default, running-dir clamp,
path safety, hidden filter, readonly enforcement.

## Pre-flight

All test state lives under `/tmp/fa_test/` — both the substrate's
`.fantastic/` directory AND the file_bridge's data root (`fa_root/`,
RELATIVE: the running-dir law clamps every root inside the dir the
kernel runs in). Nothing written to the project tree.

```bash
rm -rf /tmp/fa_test
mkdir -p /tmp/fa_test/fa_root
cd /tmp/fa_test
```

## Tests

### Test 0: sealed by default — verbs deny until the edge is opened

```bash
SEALED=$(fantastic kernel_state create_agent handler_module=file_bridge.tools root=fa_root | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $SEALED read path=anything.txt
fantastic $SEALED reflect | python -m json.tool | grep -F '"sealed": true'
fantastic kernel_state delete_agent id=$SEALED
```
Expected: the read returns `{"error":…, "reason":"unauthorized", "hint":"…ingress_rule…"}`;
reflect still answers (discovery is ungated) and shows `sealed: true`.

### Test 1: write + read round-trip (opened leg)

```bash
FA=$(fantastic kernel_state create_agent handler_module=file_bridge.tools root=fa_root ingress_rule=allow_all | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $FA write path=hello.txt content="hi there"
fantastic $FA read path=hello.txt | python -m json.tool | grep -F '"content": "hi there"'
```
Expected: grep matches.

### Test 2: write creates parent dirs

```bash
fantastic $FA write path=a/b/c.txt content=deep
test -f /tmp/fa_test/fa_root/a/b/c.txt && cat /tmp/fa_test/fa_root/a/b/c.txt
```
Expected: prints `deep`.

### Test 3: list shows files, hides defaults

```bash
mkdir /tmp/fa_test/fa_root/.git
echo visible > /tmp/fa_test/fa_root/visible.txt
fantastic $FA list path="" | python -m json.tool | grep -E "\"name\": \"(.git|visible.txt)\""
```
Expected: only `visible.txt` (the .git dir is filtered by DEFAULT_HIDDEN).

### Test 4: path safety rejects escape

```bash
fantastic $FA read path=../../etc/passwd
```
Expected: `{"error":"…escape…"}`.

### Test 5: the running-dir law — a root outside cwd refuses

```bash
ESC=$(fantastic kernel_state create_agent handler_module=file_bridge.tools root=../escape ingress_rule=allow_all | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $ESC list path=""
fantastic $ESC reflect | python -m json.tool | grep -F '"root_error"'
fantastic kernel_state delete_agent id=$ESC
```
Expected: list returns `{"error":"…escapes the running dir…"}`; reflect
still answers and carries `root_error`.

### Test 6: image read returns base64

```bash
printf '\x89PNG\r\n\x1a\nfake' > /tmp/fa_test/fa_root/img.png
fantastic $FA read path=img.png | python -m json.tool | grep -F '"image_base64"'
fantastic $FA read path=img.png | python -m json.tool | grep -F '"mime": "image/png"'
```
Expected: both greps match.

### Test 7: delete file

```bash
fantastic $FA delete path=hello.txt
test ! -f /tmp/fa_test/fa_root/hello.txt && echo OK
```
Expected: `OK`.

### Test 8: rename

```bash
echo content > /tmp/fa_test/fa_root/old.txt
fantastic $FA rename old_path=old.txt new_path=new.txt
test -f /tmp/fa_test/fa_root/new.txt && echo OK
```
Expected: `OK`.

### Test 9: mkdir recursive

```bash
fantastic $FA mkdir path=newdir/sub
test -d /tmp/fa_test/fa_root/newdir/sub && echo OK
```
Expected: `OK`.

### Test 10: readonly refuses write

```bash
fantastic kernel_state update_agent id=$FA readonly=true
fantastic $FA write path=x.txt content=x
```
Expected: `{"error":"…readonly…"}`.

### Test 11: cascade delete cleans up the file_bridge record

```bash
fantastic kernel_state delete_agent id=$FA
test ! -d /tmp/fa_test/.fantastic/agents/$FA && echo OK
```
Expected: `OK` (substrate's cascade removed the on-disk record).
The bridge's data root (`/tmp/fa_test/fa_root`) is untouched — it's not
the substrate's concern.

## Cleanup

```bash
cd /
rm -rf /tmp/fa_test
```

## Summary

| # | Test | Pass |
|---|------|------|
| 0 | sealed by default; reflect still answers | |
| 1 | write + read round-trip (opened) | |
| 2 | write creates parent dirs | |
| 3 | list with hidden filter | |
| 4 | path safety rejects ../ | |
| 5 | running-dir law refuses outside root | |
| 6 | image read returns base64 | |
| 7 | delete file | |
| 8 | rename | |
| 9 | mkdir recursive | |
| 10 | readonly refuses write | |
| 11 | cascade delete cleans up record | |
