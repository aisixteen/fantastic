# file selftest

> scopes: kernel, persistence
> requires: `uv sync`
> out-of-scope: HTTP, AI, real network filesystems

Filesystem agent. Path safety, hidden filter, readonly enforcement.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic /tmp/fa_root
mkdir /tmp/fa_root
```

## Tests

### Test 1: write + read round-trip

```bash
FA=$(uv run python kernel.py call core create_agent handler_module=file.tools root=/tmp/fa_root | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
uv run python kernel.py call $FA write path=hello.txt content="hi there"
uv run python kernel.py call $FA read path=hello.txt | python -m json.tool | grep -F '"content": "hi there"'
```
Expected: grep matches.

### Test 2: write creates parent dirs

```bash
uv run python kernel.py call $FA write path=a/b/c.txt content=deep
test -f /tmp/fa_root/a/b/c.txt && cat /tmp/fa_root/a/b/c.txt
```
Expected: prints `deep`.

### Test 3: list shows files, hides defaults

```bash
mkdir /tmp/fa_root/.git
echo visible > /tmp/fa_root/visible.txt
uv run python kernel.py call $FA list path="" | python -m json.tool | grep -E "\"name\": \"(.git|visible.txt)\""
```
Expected: only `visible.txt` (the .git dir is filtered by DEFAULT_HIDDEN).

### Test 4: path safety rejects escape

```bash
uv run python kernel.py call $FA read path=../../etc/passwd
```
Expected: `{"error":"…escape…"}`.

### Test 5: image read returns base64

```bash
printf '\x89PNG\r\n\x1a\nfake' > /tmp/fa_root/img.png
uv run python kernel.py call $FA read path=img.png | python -m json.tool | grep -F '"image_base64"'
uv run python kernel.py call $FA read path=img.png | python -m json.tool | grep -F '"mime": "image/png"'
```
Expected: both greps match.

### Test 6: delete file

```bash
uv run python kernel.py call $FA delete path=hello.txt
test ! -f /tmp/fa_root/hello.txt && echo OK
```
Expected: `OK`.

### Test 7: rename

```bash
echo content > /tmp/fa_root/old.txt
uv run python kernel.py call $FA rename old_path=old.txt new_path=new.txt
test -f /tmp/fa_root/new.txt && echo OK
```
Expected: `OK`.

### Test 8: mkdir recursive

```bash
uv run python kernel.py call $FA mkdir path=newdir/sub
test -d /tmp/fa_root/newdir/sub && echo OK
```
Expected: `OK`.

### Test 9: readonly refuses write

```bash
uv run python kernel.py call core update_agent id=$FA readonly=true
uv run python kernel.py call $FA write path=x.txt content=x
```
Expected: `{"error":"…readonly…"}`.

## Cleanup

```bash
rm -rf /tmp/fa_root .fantastic
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
