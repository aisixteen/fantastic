// e2e panel verifier: open the bare-host canvas, wait for the builder's panels to
// hydrate, dump what each shows, and PASS if any panel is displaying live workflow
// output (not a placeholder). Reuses the browser driver from the ts integration
// tests.  usage: node integration_tests/py_ts/e2e/verify_panel.ts [PORT]
import { Browser, chromeAvailable } from "../_chrome.ts";

const port = process.argv[2] ?? "8930";
const url = `http://127.0.0.1:${port}/ts_dist/file/_e2e_canvas.html`;

if (!chromeAvailable()) {
  console.log("SKIP: system Chrome not found");
  process.exit(0);
}

const b = await Browser.launch();
try {
  await b.goto(url);
  await b.waitFor("!!document.getElementById('canvas')", 20000);
  await b.waitFor("document.querySelectorAll('.agent-frame iframe').length >= 1", 20000);

  // Poll up to ~45s for a panel to show real content (a scheduled tick must land
  // while the panels are watching).
  const READ_ALL = "(() => ((document.body && document.body.innerText) || '').trim())()";
  let dumps: string[] = [];
  let hit = false;
  const deadline = Date.now() + 45000;
  while (Date.now() < deadline) {
    const raw = await b.evalAllIframes<string>(READ_ALL);
    dumps = (Array.isArray(raw) ? raw : []).map((t) => (typeof t === "string" ? t : ""));
    if (dumps.some((t) => t.length > 0 && t !== "—" && !/^mode:/i.test(t))) {
      hit = true;
      break;
    }
    await new Promise((r) => setTimeout(r, 1000));
  }

  console.log("=== panels ===");
  dumps.forEach((t, i) => console.log(`panel[${i}]: ${JSON.stringify(t.slice(0, 240))}`));
  console.log("=== page errors ===", JSON.stringify(b.pageErrors));
  if (hit) {
    console.log("PASS: a panel is showing live workflow output");
    process.exit(0);
  }
  console.log("FAIL: no panel showed output within the window");
  process.exit(1);
} finally {
  b.close();
}
