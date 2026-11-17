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
import { CANVAS_VIEW_SKINS, mountPanel } from "../src/bundles/html_agent/html_view.ts";
import { STOCK_SKINS } from "../src/bundles/skin/stock.ts";
import { skinNow } from "../src/bundles/skin/apply.ts";
import { Agent, Kernel } from "../src/lib.ts";
import type { ViewContext } from "../src/bundles/view.ts";

// THE SKINS OF AN HTML VIEW (#698): a view is see-through and declares no colour of its own and
// no token block: it draws with skin tokens only, and the host that mounts it passes the block of
// its skin — the canvas its own, a product page light or dark (`web/page/view_skins/`).

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

test("light is the day skin; dark is opaque", () => {
  const day = compileSkin(stockSkin("day")!.tokens);
  assert.ok(day.ok);
  assert.equal(viewSkinBlock("light"), day.ok ? day.skin.text : "");
  const bg = (name: (typeof VIEW_SKINS)[number]): number => {
    const c = compileSkin(viewSkinTokens(name));
    return c.ok ? (c.skin.values.get("color.page.bg") as ColorValue).alpha : -1;
  };
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
  test(`${rel}: skin tokens only — no token block of its own, no literal colour, see-through`, () => {
    const html = readFileSync(ROOT + rel, "utf8");
    const style = [...html.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)].map((m) => m[1]!).join("\n");
    assert.deepEqual([...html.matchAll(/(--fc-[\w-]+)\s*:/g)].map((m) => m[1]), [], "it declares no --fc-* value (the host passes the skin)");
    const used = [...new Set([...html.matchAll(/var\((--fc-[\w-]+)/g)].map((m) => m[1]!))];
    assert.ok(used.length > 0, "it draws with skin tokens");
    for (const name of used) assert.ok(NAMES.has(name), `${name} is not a skin token`);
    // a colour property (css, an inline style, a script's style write) set to a literal colour
    const literal = [...html.matchAll(/(?:color|background|border|fill|stroke|shadow|outline)[\w-]*["']?\s*[:=][^;"'}\n]*?(?:(?<![&\w])#[0-9a-fA-F]{3,8}\b|\b(?:rgba?|hsla?)\()/gi)].map((m) => m[0]);
    assert.deepEqual(literal, [], "no literal colour");
    assert.match(style, /body\s*\{[^}]*background:\s*transparent/, "the body paints no page: the host's cell is its backdrop");
    assert.ok(!html.includes("--fc-color-page-bg"), "the page colour is the host's, never the view's");
  });
}

test("every canvas skin has its view skin: dark and glass take dark, day takes light", () => {
  assert.deepEqual(Object.keys(CANVAS_VIEW_SKINS).sort(), STOCK_SKINS.map((s) => s.name).sort());
  assert.deepEqual(CANVAS_VIEW_SKINS, { day: "light", dark: "dark", glass: "dark" });
});

test("the canvas mounts every view with the view skin of its skin (dark by default)", async () => {
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
    assert.ok(frame.srcdoc.includes(`<style id="fantastic-skin">${viewSkinBlock("dark")}</style>`), "the dark view skin, as the team page's dark");
    assert.equal(skinNow().name, "dark", "the web default");
    h.unmount();
  } finally {
    g["document"] = was.document;
    g["window"] = was.window;
  }
});
