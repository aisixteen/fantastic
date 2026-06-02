// dawee port check: panels render + the dawee-mixer bus actually crosses the
// sandboxed iframes through the host `mixerbus` agent. (WebGL is disabled in this
// headless Chrome, so the GL viz can't render here — verify that in a real browser.)
//   usage: node integration_tests/py_ts/e2e/dawee_verify.ts
import { Browser, chromeAvailable } from "../_chrome.ts";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
if (!chromeAvailable()) {
  console.log("SKIP: no chrome");
  process.exit(0);
}
const b = await Browser.launch();
try {
  await b.goto("http://127.0.0.1:8890/ts_dist/file/_dawee_canvas.html");
  await b.waitFor("!!document.getElementById('canvas')", 20000);
  await b.waitFor("document.querySelectorAll('.agent-frame iframe').length >= 5", 20000);
  await sleep(2500); // panels boot + watches settle

  const frames = await b.evaluate<number>("document.querySelectorAll('.agent-frame').length");
  const iframes = await b.evaluate<number>("document.querySelectorAll('.agent-frame iframe').length");
  const sandbox = await b.evaluate<string>(
    "document.querySelector('.agent-frame iframe')?.getAttribute('sandbox') || '?'",
  );
  console.log(`agent-frames=${frames}  iframes=${iframes}  sandbox="${sandbox}"`);

  // ── functional bus test: install a recorder in every iframe, ping from one ──
  await b.evalAllIframes<boolean>(
    "(() => { try { window.__pings = []; const c = new BroadcastChannel('dawee-mixer'); c.onmessage = (e) => { if (e && e.data && e.data.__t) window.__pings.push(e.data.v); }; window.__rec = c; return true; } catch { return false; } })()",
  );
  await sleep(400);
  await b.evalInAnyIframe<string>(
    "(() => { new BroadcastChannel('dawee-mixer').postMessage({ __t: 1, v: 777 }); return 'sent'; })()",
    8000,
  );
  await sleep(1800); // emit → host mixerbus → fanout back to the other iframes
  const pings = await b.evalAllIframes<number[]>("(() => window.__pings || [])()");
  const received = (Array.isArray(pings) ? pings : []).filter((p) => Array.isArray(p) && p.includes(777)).length;
  console.log(`bus: ${received} of ${iframes} iframes received the cross-panel ping (sender excluded)`);

  console.log("page errors:", JSON.stringify(b.pageErrors.filter((e) => !/WebGL|GL context/i.test(e))));
  console.log(received >= 1 ? "PASS: dawee-mixer bus crosses panels through the host" : "FAIL: bus did not cross");
  process.exit(received >= 1 ? 0 : 1);
} finally {
  b.close();
}
