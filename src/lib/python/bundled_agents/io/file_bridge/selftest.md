# file_bridge selftest

> scopes: kernel, persistence
> requires: `uv sync`
> out-of-scope: HTTP, AI, real network filesystems

The fs edge of the io family. Sealed by default, running-dir clamp,
path safety, the `allow` list, text verbs + undo, readonly enforcement.

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
SEALED=$(fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools root=fa_root allow='["."]' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $SEALED read path=anything.txt
fantastic $SEALED reflect | python -m json.tool | grep -F '"sealed": true'
fantastic kernel_state delete_agent id=$SEALED
```
Expected: the read returns `{"error":…, "reason":"unauthorized", "hint":"…ingress_rule…"}`;
reflect still answers (discovery is ungated) and shows `sealed: true`.

### Test 1: write + read round-trip (opened leg)

```bash
FA=$(fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools root=fa_root allow='["."]' ingress_rule=allow_all_ingress | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $FA write path=hello.txt bytes="hi there"
fantastic $FA read path=hello.txt | python -m json.tool | grep -F '"text": "hi there"'
```
Expected: grep matches.

### Test 2: write creates parent dirs

```bash
fantastic $FA write path=a/b/c.txt bytes=deep
test -f /tmp/fa_test/fa_root/a/b/c.txt && cat /tmp/fa_test/fa_root/a/b/c.txt
```
Expected: prints `deep`.

### Test 3: list shows every exposed entry — `allow` is the only filter

```bash
mkdir /tmp/fa_test/fa_root/.git
echo visible > /tmp/fa_test/fa_root/visible.txt
fantastic $FA list path="" | python -m json.tool | grep -E "\"name\": \"(.git|visible.txt)\""
```
Expected: BOTH `.git` and `visible.txt` (`allow:["."]` exposes the whole root;
expose narrowly with named entries).

### Test 4: path safety rejects escape

```bash
fantastic $FA read path=../../etc/passwd
```
Expected: `{"error":"…escape…"}`.

### Test 5: the running-dir law — a root outside the running dir refuses

```bash
ESC=$(fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools root=../escape ingress_rule=allow_all_ingress | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
fantastic $ESC list path=""
fantastic $ESC reflect | python -m json.tool | grep -F '"root_error"'
fantastic kernel_state delete_agent id=$ESC
```
Expected: list returns `{"error":"…escapes the running dir…"}`; reflect
still answers and carries `root_error`.

### Test 6: `read` refuses binary; `edit` applies a hunk

```bash
printf '\x89PNG\r\n\x1a\nfake' > /tmp/fa_test/fa_root/img.png
fantastic $FA read path=img.png | python -m json.tool | grep -F '"reason": "bad_arg"'
fantastic $FA edit edits='[{"path":"hello.txt","hunks":1}]' bytes="$(printf '<<<<<<< SEARCH\nhi there\n=======\nbye\n>>>>>>> REPLACE\n')"
cat /tmp/fa_test/fa_root/hello.txt; echo
```
Expected: the binary read is refused (`bad_arg`, "a text file"); the file reads
`bye`. (`undo` keeps its slot in the RAM of the kernel that made the edit, so a
one-shot `fantastic` process cannot show it — drive it inside one live kernel.)

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
fantastic $FA write path=x.txt bytes=x
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
| 3 | list shows every exposed entry | |
| 4 | path safety rejects ../ | |
| 5 | running-dir law refuses outside root | |
| 6 | read refuses binary; edit applies a hunk | |
| 7 | delete file | |
| 8 | rename | |
| 9 | mkdir recursive | |
| 10 | readonly refuses write | |
| 11 | cascade delete cleans up record | |
