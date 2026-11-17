import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { SKIN_SCHEMA, cssName } from "../src/bundles/skin/schema.ts";
import { stockSkin } from "../src/bundles/skin/stock.ts";
import { compileSkin } from "../src/bundles/skin/tokens.ts";
import type { ColorValue } from "../src/bundles/skin/tokens.ts";
import { VIEW_SKINS, viewSkinBlock, viewSkinTokens } from "../src/bundles/skin/view_skins.ts";
import { mountPanel } from "../src/bundles/html_agent/html_view.ts";
import { Agent, Kernel } from "../src/lib.ts";
import type { ViewContext } from "../src/bundles/view.ts";

// THE SKINS OF AN HTML VIEW (#698): a view declares no colour of its own and no token
// block: it draws with skin tokens only, and the host that mounts it passes the block of
// the skin it shows (light, dark or transparent) — the canvas passes transparent.

const ROOT = fileURLToPath(new URL("../../../../../", import.meta.url));
const PAGE = fileURLToPath(new URL("../../page/view_skins/", import.meta.url));
const NAMES = new Set(SKIN_SCHEMA.map((t) => cssName(t.path)));

/** Every `*_view.html` the repository ships (an agent's `view {kind: html}`), relative to the root. */
function shippedViews(): string[] {
  const out: string[] = [];
  const skip = new Set(["node_modules", "target", "dist", "runs", ".git", ".venv"]);
  const walk = (dir: string): void => {
    for (const n of readdirSync(dir)) {
      if (skip.has(n)) continue;
      const p = join(dir, n);
      if (statSync(p).isDirectory()) walk(p);
      else if (n.endsWith("_view.html")) out.push(relative(ROOT, p).split("\\").join("/"));
    }
  };
  walk(join(ROOT, "src"));
  return out.sort();
}

test("each view skin compiles, every opaque token at alpha 1", () => {
  const opaque = SKIN_SCHEMA.filter((t) => t.opaque === true).map((t) => t.path);
  for (const name of VIEW_SKINS) {
    const c = compileSkin(viewSkinTokens(name));
    assert.ok(c.ok, `${name}: ${JSON.stringify(c)}`);
    if (!c.ok) continue;
    for (const p of opaque) assert.equal((c.skin.values.get(p) as ColorValue).alpha, 1, `${name}: ${p}`);
  }
});

test("light is the day skin; transparent is see-through; light and dark paint their page", () => {
  const day = compileSkin(stockSkin("day")!.tokens);
  assert.ok(day.ok);
  assert.equal(viewSkinBlock("light"), day.ok ? day.skin.text : "");
  const bg = (name: (typeof VIEW_SKINS)[number]): number => {
    const c = compileSkin(viewSkinTokens(name));
    return c.ok ? (c.skin.values.get("color.page.bg") as ColorValue).alpha : -1;
  };
  assert.equal(bg("transparent"), 0);
  assert.equal(bg("light"), 1);
  assert.equal(bg("dark"), 1);
});

test("web/page/view_skins/<name>.css is the skin's block (node scripts/view_skins.ts writes them)", () => {
  for (const name of VIEW_SKINS) assert.equal(readFileSync(`${PAGE}${name}.css`, "utf8"), viewSkinBlock(name), `${name}.css: run node scripts/view_skins.ts`);
});

const views = shippedViews();

test("the shipped views are found", () => {
  for (const v of ["src/tui/src/kern/sidebar_view.html", "src/lib/rust/crates/bundles/fantastic-ai-core/src/chat_view.html", "src/lib/rust/crates/bundles/fantastic-file/src/file_view.html"])
    assert.ok(views.includes(v), `${v} is not found among ${views.join(", ")}`);
});

for (const rel of views) {
  test(`${rel}: skin tokens only — no token block of its own, no literal colour, its page is the skin's`, () => {
    const html = readFileSync(ROOT + rel, "utf8");
    const style = [...html.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)].map((m) => m[1]!).join("\n");
    assert.deepEqual([...html.matchAll(/(--fc-[\w-]+)\s*:/g)].map((m) => m[1]), [], "it declares no --fc-* value (the host passes the skin)");
    const used = [...new Set([...html.matchAll(/var\((--fc-[\w-]+)/g)].map((m) => m[1]!))];
    assert.ok(used.length > 0, "it draws with skin tokens");
    for (const name of used) assert.ok(NAMES.has(name), `${name} is not a skin token`);
    // a colour property (css, an inline style, a script's style write) set to a literal colour
    const literal = [...html.matchAll(/(?:color|background|border|fill|stroke|shadow|outline)[\w-]*["']?\s*[:=][^;"'}\n]*?(?:(?<![&\w])#[0-9a-fA-F]{3,8}\b|\b(?:rgba?|hsla?)\()/gi)].map((m) => m[0]);
    assert.deepEqual(literal, [], "no literal colour");
    assert.match(style, /body\s*\{[^}]*background:\s*var\(--fc-color-page-bg\)/, "the body is the skin's page (see-through in transparent)");
  });
}

test("the canvas mounts every view with the transparent skin", async () => {
  const frame = { srcdoc: "", contentWindow: { postMessage: () => {} }, setAttribute: () => {}, addEventListener: () => {}, remove: () => {} };
  const g = globalThis as unknown as Record<string, unknown>;
  const was = { document: g["document"], window: g["window"] };
  g["document"] = { createElement: () => frame };
  g["window"] = { addEventListener: () => {}, removeEventListener: () => {} };
  try {
    const kernel = new Kernel();
    kernel.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
    kernel.register(new Agent({ id: "cell", parentId: "canvas" }));
    const ctx = { kernel, mount: { appendChild: () => {} }, selfId: "cell", backend: "cell", close: () => {} } as unknown as ViewContext;
    const h = mountPanel(ctx, () => "<b>x</b>");
    await new Promise((r) => setTimeout(r, 0));
    assert.ok(frame.srcdoc.includes(`<style id="fantastic-skin">${viewSkinBlock("transparent")}</style>`));
    h.unmount();
  } finally {
    g["document"] = was.document;
    g["window"] = was.window;
  }
});
