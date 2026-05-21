# html_agent selftest

> scopes: kernel, http, web
> requires: `uv sync`; `fantastic` running for HTTP tests
> out-of-scope: visual rendering / cross-iframe interactions (manual)

UI-as-a-record. The agent's `html_content` field IS the page; webapp
serves it at `/<id>/` with `fantastic_transport()` auto-injected.

## Pre-flight

```bash
cd new_codebase
rm -rf .fantastic
PORT=18906
pkill -9 -f "fantastic" 2>/dev/null
uv run --active fantastic core create_agent handler_module=web.tools port=$PORT >/dev/null
# WS verb channel is now a sub-agent of web — spawn under web's id.
WEB_ID=$(ls .fantastic/agents | grep '^web_' | head -1)
uv run --active fantastic $WEB_ID create_agent handler_module=web_ws.tools >/dev/null
uv run --active fantastic > /tmp/s.log 2>&1 &
SPID=$!
for i in $(seq 1 30); do grep -q "kernel up" /tmp/s.log 2>/dev/null && break; sleep 0.3; done

# This helper opens a one-shot WS, sends a `call` frame, prints reply.
call() {
  TARGET="$1" PAYLOAD="$2" PORT="$PORT" uv run --active python - <<'PY'
import asyncio, json, os, websockets
target = os.environ["TARGET"]; payload = json.loads(os.environ["PAYLOAD"])
port = os.environ["PORT"]
async def main():
    async with websockets.connect(f"ws://localhost:{port}/{target}/ws") as ws:
        await ws.send(json.dumps({"type":"call","target":target,"payload":payload,"id":"1"}))
        while True:
            m = json.loads(await ws.recv())
            if m.get("id") == "1" and m.get("type") in ("reply","error"):
                print(json.dumps(m.get("data"))); return
asyncio.run(main())
PY
}
```

After all tests:
```bash
kill -9 $SPID 2>/dev/null; rm -rf .fantastic /tmp/s.log
```

## Tests

### Test 1: create + reflect

```bash
HA=$(call core '{"type":"create_agent","handler_module":"html_agent.tools","html_content":"<h1>hi</h1>","display_name":"Panel"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "HA=$HA"
call $HA '{"type":"reflect"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = d['id'] == '$HA' and d['display_name'] == 'Panel' and d['html_bytes'] >= 9
print('PASS' if ok else f'FAIL d={d}')
"
```
Expected: `PASS`.

### Test 2: served HTML at /<id>/ contains record content + transport

```bash
HTML=$(curl -s "http://localhost:$PORT/$HA/")
echo "$HTML" | grep -qF "<h1>hi</h1>" && echo "  record content: OK" || echo "  record content: FAIL"
echo "$HTML" | grep -qF "_fantastic/transport.js" && echo "  transport injected: OK; PASS" || echo "  transport injected: FAIL"
```
Expected: both checks PASS.
Regression signal: missing `<h1>hi</h1>` → `render_html` duck-type
broke. Missing `transport.js` → the webapp's `_inject` regressed.

### Test 3: set_html updates record + emits reload_html

```bash
call $HA '{"type":"set_html","html":"<p>v2</p>"}' | python -m json.tool | grep -F '"ok": true'
curl -s "http://localhost:$PORT/$HA/" | grep -qF "<p>v2</p>" && echo "  served HTML updated: OK; PASS" || echo "  FAIL"
```
Expected: `"ok": true` and "served HTML updated: OK".
The emitted `reload_html` event is consumed by transport.js's
universal listener — any open browser tab on `/<HA>/` calls
`location.reload()` automatically. Verifying that auto-reload requires
the browser; here we only assert the verb wrote the new content.

### Test 4: placeholder served when html_content unset

```bash
HA2=$(call core '{"type":"create_agent","handler_module":"html_agent.tools"}' | python -c "import json,sys;print(json.load(sys.stdin)['id'])")
HTML=$(curl -s "http://localhost:$PORT/$HA2/")
echo "$HTML" | grep -qF "$HA2" && echo "$HTML" | grep -qF "set_html" && echo "PASS" || echo "FAIL"
```
Expected: `PASS`.

### Test 5: get_webapp descriptor matches record

```bash
call $HA '{"type":"get_webapp"}' | python -c "
import json, sys
d = json.loads(sys.stdin.read(), strict=False)
ok = d['url'] == '/$HA/' and d['title'] == 'Panel' and d['default_width'] == 480
print('PASS' if ok else f'FAIL d={d}')
"
```
Expected: `PASS`.

### Test 6: set_html rejects non-string

```bash
call $HA '{"type":"set_html","html":42}' | grep -qF "must be a string" && echo "PASS" || echo "FAIL"
```

### Test 7 (manual, browser): cross-agent call from inside the iframe

Set this html_content (paste into a `set_html` curl):
```html
<!doctype html><body>
<button id="b">List agents</button>
<pre id="out"></pre>
<script>
const t = fantastic_transport(); await t.ready;
document.getElementById('b').onclick = async () => {
  const r = await t.call('core', {type:'list_agents'});
  document.getElementById('out').textContent = r.agents.map(a => a.id).join('\n');
};
</script>
</body>
```
Then open `/<HA>/` in a browser, click. Expected: list of running
agent ids appears in the `<pre>`. Confirms the html_agent can call
ANY verb on ANY other agent through the auto-injected transport.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | create + reflect | |
| 2 | served HTML carries record + transport | |
| 3 | set_html updates served body | |
| 4 | placeholder when unset | |
| 5 | get_webapp descriptor | |
| 6 | set_html rejects non-string | |
| 7 (manual) | cross-agent call from iframe | |
