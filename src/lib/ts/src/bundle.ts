// Rolled-up bundle entry — the esbuild entrypoint for `dist/js_kernel.zip`
// (see ../scripts/pack.sh, ../readme.md). It exists ONLY for the bundle build:
//
//   - The dev/importmap build (`npm run build`, entry `main.ts`) links
//     `vendor/xterm.css` as an external <link> and resolves bare `three` /
//     `@xterm/*` specifiers via an HTML import map. `main.ts` is its entry.
//   - The bundle build (esbuild, entry THIS file) has no HTML scaffold: every
//     vendor byte + the css must live inside the single output file. So here we
//     inline `xterm.css` via esbuild's `text` loader, inject it once, then load
//     the canvas bootstrap. esbuild's `--alias` maps the bare vendor specifiers
//     to `vendor/*.js`; with no `--splitting` the dynamic imports inline into
//     the one file.
//
// This file is EXCLUDED from the dev tsconfig (the css import is esbuild-only);
// esbuild does not typecheck, so the untyped css default import is fine here.
import xtermCss from "./vendor/xterm.css";

const style = document.createElement("style");
style.dataset.fantasticVendor = "xterm";
style.textContent = xtermCss;
document.head.appendChild(style);

// Lazy so the css <style> lands before the canvas (and any terminal view) mounts.
await import("./main.ts");
