// Rolled-up bundle entry — the esbuild entrypoint for this app's zip (see
// ../scripts/pack.sh, ../readme.md). It exists ONLY for the bundle build: the
// bundle has no HTML scaffold, so every vendor byte + the css must live inside
// the single output file. Here `xterm.css` is inlined via esbuild's `text`
// loader and injected once, then the page loads (the canvas, `main.ts`, or the Home, `home.ts`). esbuild's
// `--alias` maps the bare vendor specifiers (`three`, `@xterm/*`) to
// `vendor/*.js`; with no `--splitting` the dynamic imports inline into the one
// file.
//
// This file is EXCLUDED from the tsconfig (the css and html imports are esbuild-only);
// esbuild does not typecheck, so the untyped css default import is fine here.
import xtermCss from "./vendor/xterm.css";

const style = document.createElement("style");
style.dataset.fantasticVendor = "xterm";
style.textContent = xtermCss;
document.head.appendChild(style);

// Lazy so the css <style> lands before the page mounts. Which page is the
// mount page's to say: `home.html` is `<body data-page="home">` (the Home,
// `home.ts`); every other page is the canvas (`main.ts`). Two literal imports,
// so esbuild inlines both.
if (document.body.dataset["page"] === "home") await import("./home.ts");
else await import("./main.ts");
