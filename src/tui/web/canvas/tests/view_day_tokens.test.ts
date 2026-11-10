import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { SCHEMA_BY_PATH, cssName } from "../src/bundles/skin/schema.ts";
import { stockSkin } from "../src/bundles/skin/stock.ts";
import { compileSkin } from "../src/bundles/skin/tokens.ts";

// THE DAY BLOCK OF AN HTML VIEW. A view runs in a null-origin iframe and gets no
// skin, so each `*_view.html` a bundle ships carries its own `:root{--fc-…}`
// block. This keeps that block equal to the day skin, token by token.

/** Paths of the shipped `*_view.html` files, relative to the repository root. */
const VIEWS: string[] = [];

const ROOT = fileURLToPath(new URL("../../../../../", import.meta.url));

const day = compileSkin(stockSkin("day")!.tokens);
const byName = new Map(day.ok ? [...day.skin.css].map(([path, css]) => [cssName(path), { path, css }]) : []);

test("the day skin compiles", () => {
  assert.ok(day.ok, JSON.stringify(day));
});

for (const rel of VIEWS) {
  test(`${rel}: its :root block is the day skin's, and every var(--fc-…) it uses is declared`, () => {
    assert.ok(existsSync(ROOT + rel), `${rel} does not exist`);
    const html = readFileSync(ROOT + rel, "utf8");
    const style = [...html.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)].map((m) => m[1]!).join("\n");
    const roots = style.match(/:root\s*\{[^}]*\}/g) ?? [];
    assert.equal(roots.length, 1, "one :root block");
    const decls = [...roots[0]!.matchAll(/(--fc-[\w-]+)\s*:\s*([^;]+);/g)];
    assert.ok(decls.length > 0, "the block declares --fc-* values");
    for (const [, name, value] of decls) {
      const token = byName.get(name!);
      assert.ok(token !== undefined && SCHEMA_BY_PATH.has(token.path), `${name} is not a schema property`);
      assert.equal(value!.trim(), token.css, `${name} is the day skin's`);
    }
    const defined = new Set(decls.map((d) => d[1]));
    const used = new Set([...html.replace(roots[0]!, "").matchAll(/var\((--fc-[\w-]+)/g)].map((m) => m[1]!));
    for (const name of used) assert.ok(defined.has(name), `${name} is used but not declared in the :root block`);
  });
}
