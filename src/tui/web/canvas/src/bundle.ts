// Rolled-up bundle entry — the esbuild entrypoint for this app's zip (see
// ../scripts/pack.sh, ../readme.md). It exists ONLY for the bundle build: the
// bundle has no HTML scaffold, so every vendor byte + the css must live inside
// the single output file. Here `xterm.css` is inlined via esbuild's `text`
// loader and injected once, then the canvas page (`main.ts`) loads. esbuild's
// `--alias` maps the bare vendor specifiers (`three`, `@xterm/*`) to
// `vendor/*.js`; with no `--splitting` the dynamic imports inline into the one
// file.
//
// This file is EXCLUDED from the tsconfig (the css and html imports are esbuild-only);
// esbuild does not typecheck, so the untyped css default import is fine here.
import xtermCss from "./vendor/xterm.css";
// the demo page, packed first by ../scripts/pack.sh (one self-contained html)
import demoHtml from "../../../demo/web/dist/index.html";
import { setDemoHtml } from "./demo_page.ts";

setDemoHtml(demoHtml);

const style = document.createElement("style");
style.dataset.fantasticVendor = "xterm";
style.textContent = xtermCss;
document.head.appendChild(style);

// Lazy so the css <style> lands before the canvas (and any terminal view) mounts.
await import("./main.ts");
