# file selftest

> scopes: kernel, persistence
> requires: `uv sync`
> out-of-scope: HTTP, AI, real network filesystems

Filesystem agent. Path safety, hidden filter, readonly enforcement.

## Pre-flight

All test state lives under `/tmp/fa_test/` — both the substrate's
`.fantastic/` directory AND the file agent's data root. Nothing
written to the project tree.

```bash
rm -rf /tmp/fa_test /tmp/fa_root
mkdir -p /tmp/fa_test /tmp/fa_root
cd /tmp/fa_test
```

## Tests

### Test 1: write + read round-trip

```bash
FA=$(fantastic call core create_agent handler_module=file.tools root=/tmp/fa_root | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic call $FA write path=hello.txt content="hi there"
fantastic call $FA read path=hello.txt | python -m json.tool | grep -F '"content": "hi there"'
```
Expected: grep matches.

### Test 2: write creates parent dirs

```bash
fantastic call $FA write path=a/b/c.txt content=deep
test -f /tmp/fa_root/a/b/c.txt && cat /tmp/fa_root/a/b/c.txt
```
Expected: prints `deep`.

### Test 3: list shows files, hides defaults

```bash
mkdir /tmp/fa_root/.git
echo visible > /tmp/fa_root/visible.txt
fantastic call $FA list path="" | python -m json.tool | grep -E "\"name\": \"(.git|visible.txt)\""
```
Expected: only `visible.txt` (the .git dir is filtered by DEFAULT_HIDDEN).

### Test 4: path safety rejects escape

```bash
fantastic call $FA read path=../../etc/passwd
```
Expected: `{"error":"…escape…"}`.

### Test 5: image read returns base64

```bash
printf '\x89PNG\r\n\x1a\nfake' > /tmp/fa_root/img.png
fantastic call $FA read path=img.png | python -m json.tool | grep -F '"image_base64"'
fantastic call $FA read path=img.png | python -m json.tool | grep -F '"mime": "image/png"'
```
Expected: both greps match.

### Test 6: delete file

```bash
fantastic call $FA delete path=hello.txt
test ! -f /tmp/fa_root/hello.txt && echo OK
```
Expected: `OK`.

### Test 7: rename

```bash
echo content > /tmp/fa_root/old.txt
fantastic call $FA rename old_path=old.txt new_path=new.txt
test -f /tmp/fa_root/new.txt && echo OK
```
Expected: `OK`.

### Test 8: mkdir recursive

```bash
fantastic call $FA mkdir path=newdir/sub
test -d /tmp/fa_root/newdir/sub && echo OK
```
Expected: `OK`.

### Test 9: readonly refuses write

```bash
fantastic call core update_agent id=$FA readonly=true
fantastic call $FA write path=x.txt content=x
```
Expected: `{"error":"…readonly…"}`.

### Test 10: cascade delete cleans up the file agent record

```bash
fantastic call core delete_agent id=$FA
test ! -d /tmp/fa_test/.fantastic/agents/$FA && echo OK
```
Expected: `OK` (substrate's cascade removed the on-disk record).
The file agent's data root (`/tmp/fa_root`) is untouched — it's not
the substrate's concern.

## Cleanup

```bash
cd /
rm -rf /tmp/fa_test /tmp/fa_root
```

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | write + read round-trip | |
| 2 | write creates parent dirs | |
| 3 | list with hidden filter | |
| 4 | path safety rejects ../ | |
| 5 | image read returns base64 | |
| 6 | delete file | |
| 7 | rename | |
| 8 | mkdir recursive | |
| 9 | readonly refuses write | |
| 10 | cascade delete cleans up record | |
