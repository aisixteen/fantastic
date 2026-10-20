import { test } from "node:test";
import assert from "node:assert/strict";
import { isWebKitGtkUa, resolveRenderer } from "../src/bundles/terminal_view/renderer.ts";
import type { FantasticShell } from "../src/bundles/shell.ts";

// THE TERMINALS' RENDERER (#52): auto is DOM in WebKitGTK — by the shell's flag,
// else the user agent — and the shared WebGL everywhere else.

const GTK = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
const CHROME = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
const EDGE = `${CHROME} Edg/141.0.0.0`;
const FIREFOX = "Mozilla/5.0 (X11; Linux x86_64; rv:143.0) Gecko/20100101 Firefox/143.0";
const SAFARI = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
// headless Chrome on Linux, as the e2e harness runs it (E88's Chrome lane)
const HEADLESS = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/141.0.0.0 Safari/537.36";
// WebKitGTK as Ubuntu builds it
const GTK_UBUNTU = "Mozilla/5.0 (X11; Ubuntu; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/60.5 Safari/605.1.15";
const ANDROID = "Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/141.0.0.0 Mobile Safari/537.36";

const shell = (webview: FantasticShell["webview"], os: FantasticShell["os"]): FantasticShell => Object.freeze({ name: "tauri", os, webview, backdrop: "web" });

test("isWebKitGtkUa: WebKitGTK yes; Chrome, Edge, Firefox, Safari and Android no", () => {
  assert.equal(isWebKitGtkUa(GTK), true);
  assert.equal(isWebKitGtkUa(GTK_UBUNTU), true);
  for (const ua of [CHROME, HEADLESS, EDGE, FIREFOX, SAFARI, ANDROID]) assert.equal(isWebKitGtkUa(ua), false, ua);
});

test("resolveRenderer: a set value wins; auto follows the shell's webview over the user agent; auto without a shell follows the user agent; each answer says why", () => {
  // a set value wins over the shell and the user agent
  assert.deepEqual(resolveRenderer("webgl", { shell: shell("webkitgtk", "linux"), ua: GTK }), { in_use: "webgl", why: "set: webgl" });
  assert.deepEqual(resolveRenderer("dom", { shell: null, ua: CHROME }), { in_use: "dom", why: "set: dom" });
  // the shell's webview decides over the user agent, both ways
  assert.deepEqual(resolveRenderer("auto", { shell: shell("webkitgtk", "linux"), ua: CHROME }), { in_use: "dom", why: "auto: the shell's webview is webkitgtk" });
  assert.deepEqual(resolveRenderer("auto", { shell: shell("wkwebview", "macos"), ua: GTK }), { in_use: "webgl", why: "auto: the shell's webview is wkwebview" });
  assert.deepEqual(resolveRenderer("auto", { shell: shell("webview2", "windows"), ua: EDGE }), { in_use: "webgl", why: "auto: the shell's webview is webview2" });
  // no shell: the user agent
  assert.deepEqual(resolveRenderer("auto", { shell: null, ua: GTK }), { in_use: "dom", why: "auto: the user agent is WebKitGTK's" });
  assert.deepEqual(resolveRenderer("auto", { shell: null, ua: CHROME }), { in_use: "webgl", why: "auto: not WebKitGTK" });
});

test("resolveRenderer throws on a value that is not auto, webgl or dom", () => {
  assert.throws(() => resolveRenderer("canvas", { shell: null, ua: CHROME }), { message: "terminal_renderer: canvas is not auto, webgl or dom" });
  assert.throws(() => resolveRenderer(3, { shell: null, ua: CHROME }), { message: "terminal_renderer: 3 is not auto, webgl or dom" });
});
