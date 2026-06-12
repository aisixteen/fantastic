# fantastic-file selftest

> scopes: persistence, fs
> requires: `cargo build --release --bin fantastic`
> out-of-scope: HTTP file proxy (covered by web selftest)

Filesystem-as-agent. Path safety, hidden-file filter, read-only
enforcement when `readonly=true` on the record.

## Pre-flight

All test state lives under `/tmp/ff_test/` — both the substrate's
`.fantastic/` dir AND the file agent's data root.

```bash
rm -rf /tmp/ff_test /tmp/ff_root
mkdir -p /tmp/ff_test /tmp/ff_root
cd /tmp/ff_test
FANTASTIC=/path/to/rust/target/release/fantastic
$FANTASTIC core create_agent handler_module=file_bridge.tools id=ff root=/tmp/ff_root
```

## Tests

### Test 1: write + read round-trip

```bash
$FANTASTIC ff write path=hello.txt content="world"
$FANTASTIC ff read path=hello.txt | jq -e '.content == "world"'
```

### Test 2: list returns expected entries

```bash
$FANTASTIC ff mkdir path=sub
$FANTASTIC ff write path=sub/inner.txt content="x"
$FANTASTIC ff list path=. | jq -e '[.files[].name] | contains(["hello.txt","sub"])'
```

### Test 3: path-escape refused

```bash
! $FANTASTIC ff read path=../escape 2>&1 | grep -q "ok"
$FANTASTIC ff read path=../escape | jq -e '.error | contains("path")'
```

Expect: any `..` segment that would escape `root` is refused with a
clear error.

### Test 4: rename + delete

```bash
$FANTASTIC ff rename src=hello.txt dst=hello2.txt
$FANTASTIC ff delete path=hello2.txt
$FANTASTIC ff list path=. | jq -e '[.files[].name] | inside(["sub"])'
```

### Test 5: readonly=true blocks writes

```bash
$FANTASTIC core update_agent id=ff readonly=true
! $FANTASTIC ff write path=blocked.txt content="x"
$FANTASTIC ff write path=blocked.txt content="x" | jq -e '.error | contains("readonly")'
test ! -f /tmp/ff_root/blocked.txt
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. write + read |  |  |
| 2. list |  |  |
| 3. path-escape refused |  |  |
| 4. rename + delete |  |  |
| 5. readonly blocks writes |  |  |
