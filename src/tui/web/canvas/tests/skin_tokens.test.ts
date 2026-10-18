import { test } from "node:test";
import assert from "node:assert/strict";
import { compileSkin, mergeTokens, parseTokens } from "../src/bundles/skin/tokens.ts";
import type { ColorValue, Measure, ShadowLayer } from "../src/bundles/skin/tokens.ts";
import { SKIN_SCHEMA } from "../src/bundles/skin/schema.ts";
import type { SchemaToken } from "../src/bundles/skin/schema.ts";
import { STOCK_SKINS, stockSkin } from "../src/bundles/skin/stock.ts";

// THE TOKEN READER AND COMPILER (phase 4, #49): a strict subset of the W3C
// Design Tokens Format 2025.10 — what it does not support is refused by name.

type Dict = Record<string, unknown>;
const EXT = { $extensions: { fantastic: { backdrop: "web" } } };
const doc = (body: Dict): Dict => ({ ...EXT, ...body });
const srgb = (r: number, g: number, b: number, extra: Dict = {}): Dict => ({ colorSpace: "srgb", components: [r, g, b], ...extra });
const px = (value: number): Dict => ({ value, unit: "px" });

function ok(r: ReturnType<typeof parseTokens>): Extract<ReturnType<typeof parseTokens>, { ok: true }> {
  assert.ok(r.ok, JSON.stringify(r));
  return r as Extract<ReturnType<typeof parseTokens>, { ok: true }>;
}
function refused(r: { ok: boolean }, path: string, why: RegExp): void {
  assert.equal(r.ok, false, `expected a refusal at ${path}`);
  const e = r as unknown as { error: string; reason: string; arg: string; path: string };
  assert.equal(e.reason, "bad_arg");
  assert.equal(e.arg, "tokens");
  assert.equal(e.path, path, e.error);
  assert.match(e.error, why);
  assert.ok(e.error.startsWith(`skin: ${path} — `), e.error);
}

test("a token document parses: groups, inherited $type, $description and $extensions kept", () => {
  const r = ok(
    parseTokens(
      doc({
        $description: "a skin",
        color: {
          $type: "color",
          $description: "colours",
          $extensions: { "org.example": { x: 1 } },
          accent: { base: { $value: srgb(1, 0, 0), $description: "the accent", $extensions: { "org.example": true }, $deprecated: true } },
        },
        size: { $type: "dimension", s: { $value: px(4) }, n: { $type: "number", $value: 2 } },
      }),
    ),
  );
  assert.equal(r.backdrop, "web");
  assert.equal(r.tokens.get("color.accent.base")?.type, "color");
  assert.equal((r.tokens.get("color.accent.base")?.value as ColorValue).hex, "#ff0000");
  assert.equal(r.tokens.get("size.s")?.type, "dimension");
  assert.equal(r.tokens.get("size.n")?.type, "number", "a child's own $type wins over its group's");
  // the backdrop extension is required
  refused(parseTokens({ color: {} }), "(root)", /\$extensions/);
  refused(parseTokens({ $extensions: { fantastic: { backdrop: "glass" } } }), "$extensions.fantastic.backdrop", /web.*native/);
});

test("an object with $value and children is refused; names with { } . or a leading $ are refused", () => {
  refused(parseTokens(doc({ color: { $type: "color", a: { $value: srgb(0, 0, 0), b: { $value: srgb(0, 0, 0) } } } })), "color.a", /cannot hold children/);
  refused(parseTokens(doc({ color: { $type: "color", "a{b": { $value: srgb(0, 0, 0) } } })), "color.a{b", /\{ \} or \./);
  refused(parseTokens(doc({ color: { $type: "color", "a.b": { $value: srgb(0, 0, 0) } } })), "color.a.b", /\{ \} or \./);
  refused(parseTokens(doc({ color: { $type: "color", $odd: { $value: srgb(0, 0, 0) } } })), "color", /unknown group property \$odd/);
  refused(parseTokens(doc({ color: { $type: "color", Accent: { $value: srgb(0, 0, 0) } } })), "color.Accent", /lowercase/);
  refused(parseTokens(doc({ color: { $type: "color", a: 3 } })), "color.a", /neither a token/);
});

test("colour: srgb components and alpha are range-checked; hex must agree with the components; other colour spaces and `none` are refused", () => {
  const c = (v: unknown) => parseTokens(doc({ color: { $type: "color", a: { $value: v } } }));
  const r = ok(c(srgb(0.0588, 0.0588, 0.098, { alpha: 0.88, hex: "#0f0f19" })));
  assert.deepEqual(r.tokens.get("color.a")?.value, { hex: "#0f0f19", alpha: 0.88, css: "rgb(15 15 25 / 0.88)", components: [0.0588, 0.0588, 0.098] });
  refused(c(srgb(2, 0, 0)), "color.a", /components must be three numbers in \[0, 1\]/);
  refused(c({ colorSpace: "srgb", components: [0, 0] }), "color.a", /three numbers/);
  refused(c(srgb(0, 0, 0, { alpha: 1.5 })), "color.a", /alpha/);
  refused(c(srgb(1, 0, 0, { hex: "#00ff00" })), "color.a", /does not match the components \(#ff0000\)/);
  refused(c(srgb(1, 0, 0, { hex: "#f00" })), "color.a", /6-digit/);
  refused(c({ colorSpace: "display-p3", components: [1, 0, 0] }), "color.a", /only srgb in this canvas/);
  refused(c("none"), "color.a", /none keyword/);
});

test("dimension, duration, cubicBezier, fontFamily, fontWeight (aliases → numbers), number and shadow (object, array, inset) parse; malformed values are refused naming the path", () => {
  const r = ok(
    parseTokens(
      doc({
        d: { $type: "dimension", $value: { value: 1.5, unit: "rem" } },
        t: { $type: "duration", $value: { value: 0.2, unit: "s" } },
        e: { $type: "cubicBezier", $value: [0.42, 0, 0.58, 1] },
        f: { $type: "fontFamily", $value: "Menlo" },
        w: { $type: "fontWeight", $value: "semi-bold" },
        w2: { $type: "fontWeight", $value: 350 },
        n: { $type: "number", $value: 0.5 },
        s1: { $type: "shadow", $value: { color: srgb(0, 0, 0, { alpha: 0.5 }), offsetX: px(0), offsetY: px(2), blur: px(4), spread: px(0) } },
        s2: { $type: "shadow", $value: [{ color: srgb(1, 1, 1), offsetX: px(0), offsetY: px(1), blur: px(0), spread: px(0), inset: true }, { color: "{c}", offsetX: px(0), offsetY: px(0), blur: "{d}", spread: px(1) }] },
        c: { $type: "color", $value: srgb(0, 0, 1) },
      }),
    ),
  );
  assert.deepEqual(r.tokens.get("d")?.value, { value: 1.5, unit: "rem" });
  assert.deepEqual(r.tokens.get("f")?.value, ["Menlo"]);
  assert.equal(r.tokens.get("w")?.value, 600);
  assert.equal(r.tokens.get("w2")?.value, 350);
  const s2 = r.tokens.get("s2")?.value as ShadowLayer[];
  assert.equal(s2.length, 2);
  assert.equal(s2[0]!.inset, true);
  assert.equal(s2[1]!.color.hex, "#0000ff", "a shadow member may be an alias of its type");
  assert.deepEqual(s2[1]!.blur, { value: 1.5, unit: "rem" });
  const one = (type: string, v: unknown) => parseTokens(doc({ x: { $type: type, $value: v } }));
  refused(one("dimension", { value: 1, unit: "em" }), "x", /px or rem/);
  refused(one("dimension", 4), "x", /\{value: number/);
  refused(one("duration", { value: 1, unit: "px" }), "x", /ms or s/);
  refused(one("cubicBezier", [1.2, 0, 0.5, 1]), "x", /x1 and x2/);
  refused(one("cubicBezier", [0, 0, 1]), "x", /four numbers/);
  refused(one("fontFamily", []), "x", /fontFamily/);
  refused(one("fontWeight", "chunky"), "x", /fontWeight/);
  refused(one("fontWeight", 1200), "x", /fontWeight/);
  refused(one("number", "2"), "x", /JSON number/);
  refused(one("shadow", { color: srgb(0, 0, 0), offsetX: px(0), offsetY: px(0), blur: px(0) }), "x.spread", /needs spread/);
  refused(one("shadow", [{ color: srgb(0, 0, 0), offsetX: px(0), offsetY: px(0), blur: px(0), spread: px(0), inset: "yes" }, {}]), "x[0].inset", /boolean/);
  refused(parseTokens(doc({ x: { $value: 3 } })), "x", /needs a \$type/);
});

test("an alias resolves through a chain and takes its target's type; a cycle is refused naming every token in it; an alias to a group or a missing path is refused", () => {
  const r = ok(
    parseTokens(
      doc({
        palette: { sea: { $type: "color", $value: srgb(0.2, 0.7, 0.9) } },
        color: { a: { $value: "{palette.sea}" }, b: { $value: "{color.a}" } },
      }),
    ),
  );
  assert.equal(r.tokens.get("color.b")?.type, "color", "an alias takes its target's type");
  assert.equal((r.tokens.get("color.b")?.value as ColorValue).hex, "#33b3e6");
  const cyc = parseTokens(doc({ x: { a: { $value: "{x.b}" }, b: { $value: "{x.c}" }, c: { $value: "{x.a}" } } }));
  assert.equal(cyc.ok, false);
  const e = (cyc as { error: string }).error;
  for (const p of ["x.a", "x.b", "x.c"]) assert.ok(e.includes(p), `the cycle names ${p}: ${e}`);
  assert.match(e, /cycle/);
  refused(parseTokens(doc({ g: { $type: "color", a: { $value: srgb(0, 0, 0) } }, x: { $value: "{g}" } })), "x", /names a group/);
  refused(parseTokens(doc({ x: { $value: "{nowhere.at.all}" } })), "x", /names no token/);
  refused(parseTokens(doc({ c: { $type: "color", $value: srgb(0, 0, 0) }, x: { $type: "dimension", $value: "{c}" } })), "x", /is a color/);
});

test("$ref, $extends, $root and the unsupported types are refused by name", () => {
  refused(parseTokens(doc({ x: { $ref: "#/color/a" } })), "x", /\$ref/);
  refused(parseTokens(doc({ g: { $extends: "{other}", a: { $type: "number", $value: 1 } } })), "g", /\$extends/);
  refused(parseTokens(doc({ g: { $root: { $type: "number", $value: 1 } } })), "g", /\$root/);
  for (const t of ["strokeStyle", "border", "transition", "gradient", "typography"]) {
    refused(parseTokens(doc({ x: { $type: t, $value: {} } })), "x", new RegExp(`\\$type ${t} is not supported`));
  }
  refused(parseTokens(doc({ x: { $type: "colour", $value: {} } })), "x", /unknown \$type/);
});

/** A small schema for compile tests. */
const MINI: SchemaToken[] = [
  { path: "color.a", type: "color", opaque: true, note: "" },
  { path: "color.b", type: "color", note: "" },
  { path: "size.r", type: "dimension", note: "" },
];
const mini = (over: Dict = {}): Dict =>
  mergeTokens(doc({ color: { $type: "color", a: { $value: srgb(1, 0, 0) }, b: { $value: srgb(0, 0, 0, { alpha: 0.5 }) } }, size: { r: { $type: "dimension", $value: px(4) } } }), over);

test("compile serializes each type exactly", () => {
  const d = doc({
    color: { $type: "color", a: { $value: srgb(0.7176, 0.549, 1) }, b: { $value: srgb(0.0588, 0.0588, 0.098, { alpha: 0.88 }) } },
    r: { $type: "dimension", $value: px(16) },
    t: { $type: "duration", $value: { value: 150, unit: "ms" } },
    e: { $type: "cubicBezier", $value: [0.25, 0.1, 0.25, 1] },
    f: { $type: "fontFamily", $value: ["system-ui", "-apple-system", "BlinkMacSystemFont", "Segoe UI", "sans-serif"] },
    s: { $type: "shadow", $value: [{ color: srgb(0, 0, 0, { alpha: 0.4 }), offsetX: px(0), offsetY: px(8), blur: px(32), spread: px(0) }, { color: srgb(1, 1, 1, { alpha: 0.04 }), offsetX: px(0), offsetY: px(1), blur: px(0), spread: px(0), inset: true }] },
  });
  const schema: SchemaToken[] = [
    { path: "color.a", type: "color", note: "" },
    { path: "color.b", type: "color", note: "" },
    { path: "r", type: "dimension", note: "" },
    { path: "t", type: "duration", note: "" },
    { path: "e", type: "cubicBezier", note: "" },
    { path: "f", type: "fontFamily", note: "" },
    { path: "s", type: "shadow", note: "" },
  ];
  const c = compileSkin(d, schema);
  assert.ok(c.ok, JSON.stringify(c));
  if (!c.ok) return;
  assert.deepEqual(Object.fromEntries(c.skin.css), {
    "color.a": "rgb(183 140 255)",
    "color.b": "rgb(15 15 25 / 0.88)",
    r: "16px",
    t: "150ms",
    e: "cubic-bezier(0.25, 0.1, 0.25, 1)",
    f: 'system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
    s: "0px 8px 32px 0px rgb(0 0 0 / 0.4), inset 0px 1px 0px 0px rgb(255 255 255 / 0.04)",
  });
});

test("the stylesheet is :root { --fc-… } in schema order; two paths with one CSS name are refused", () => {
  const c = compileSkin(mini(), MINI);
  assert.ok(c.ok, JSON.stringify(c));
  if (!c.ok) return;
  assert.equal(c.skin.text, ":root {\n  --fc-color-a: rgb(255 0 0);\n  --fc-color-b: rgb(0 0 0 / 0.5);\n  --fc-size-r: 4px;\n}\n");
  assert.equal(c.skin.backdrop, "web");
  // `size.a-b` and `size.a.b` are both --fc-size-a-b
  const twin = compileSkin(mini({ size: { "a-b": { $type: "dimension", $value: px(1) }, a: { b: { $type: "dimension", $value: px(2) } } } }), MINI);
  assert.equal(twin.ok, false);
  assert.match((twin as { error: string }).error, /both --fc-size-a-b/);
});

test("a skin missing a schema token, holding an unknown token, or giving an opaque token alpha < 1 is refused; palette.* is allowed and not emitted", () => {
  const missing = doc({ color: { $type: "color", a: { $value: srgb(1, 0, 0) }, b: { $value: srgb(0, 0, 0) } } });
  refused(compileSkin(missing, MINI), "size.r", /missing/);
  refused(compileSkin(mini({ color: { foo: { $type: "color", $value: srgb(0, 0, 0) } } }), MINI), "color.foo", /not a token this canvas reads: color\.foo/);
  refused(compileSkin(mini({ color: { a: { $value: srgb(1, 0, 0, { alpha: 0.5 }) } } }), MINI), "color.a", /alpha must be 1/);
  refused(compileSkin(mini({ size: { r: { $type: "duration", $value: { value: 1, unit: "s" } } } }), MINI), "size.r", /is a duration/);
  const pal = compileSkin(mini({ palette: { "Sea Green": { $type: "color", $value: srgb(0, 1, 0) } }, color: { b: { $value: "{palette.Sea Green}" } } }), MINI);
  assert.ok(pal.ok, JSON.stringify(pal));
  if (!pal.ok) return;
  assert.equal(pal.skin.css.get("color.b"), "rgb(0 255 0)");
  assert.ok(!pal.skin.text.includes("palette"), "palette is never emitted");
});

test("mergeTokens: a partial skin over a base compiles; an over-token replaces the base's whole", () => {
  const base = mini();
  const over = { color: { b: { $value: srgb(0, 0, 1) } } };
  const m = mergeTokens(base, over);
  const c = compileSkin(m, MINI);
  assert.ok(c.ok, JSON.stringify(c));
  if (!c.ok) return;
  assert.equal(c.skin.css.get("color.b"), "rgb(0 0 255)", "the base's alpha 0.5 is gone with its whole token");
  assert.equal(c.skin.css.get("color.a"), "rgb(255 0 0)", "the rest of the base stays");
  assert.equal(c.skin.backdrop, "web", "the base's backdrop is kept");
  const native = compileSkin(mergeTokens(base, { $extensions: { fantastic: { backdrop: "native" } } }), MINI);
  assert.ok(native.ok && native.skin.backdrop === "native", "a skin may set its own");
  // the base is not changed
  assert.equal(((base["color"] as Dict)["b"] as Dict)["$value"] !== undefined, true);
  assert.deepEqual((((base["color"] as Dict)["b"] as Dict)["$value"] as Dict)["alpha"], 0.5);
});

test("the stock skins compile: dark is web, glass is native; dark's values are today's literals", () => {
  assert.deepEqual(STOCK_SKINS.map((s) => s.name), ["dark", "glass"]);
  const dark = compileSkin(stockSkin("dark")!.tokens);
  const glass = compileSkin(stockSkin("glass")!.tokens);
  assert.ok(dark.ok, JSON.stringify(dark));
  assert.ok(glass.ok, JSON.stringify(glass));
  if (!dark.ok || !glass.ok) return;
  assert.equal(dark.skin.backdrop, "web");
  assert.equal(glass.skin.backdrop, "native");
  assert.equal(dark.skin.css.size, SKIN_SCHEMA.length);
  assert.equal(SKIN_SCHEMA.length, 90);
  const css = (p: string): string | undefined => dark.skin.css.get(p);
  assert.equal(css("color.surface.bezel"), "rgb(15 15 25 / 0.88)");
  assert.equal(css("color.page.bg"), "rgb(6 6 12)");
  assert.equal(css("color.accent.base"), "rgb(183 140 255)");
  assert.equal(css("color.terminal.fg"), "rgb(229 229 229)");
  assert.equal(css("color.terminal.bg"), "rgb(0 0 0 / 0)");
  assert.equal(css("radius.bezel"), "16px");
  assert.equal(css("font.family.mono"), "ui-monospace, SFMono-Regular, Menlo, monospace");
  assert.equal(css("font.family.terminal"), '"SF Mono", Menlo, monospace');
  assert.equal(css("shadow.bezel"), "0px 8px 32px 0px rgb(0 0 0 / 0.4), inset 0px 1px 0px 0px rgb(255 255 255 / 0.04)");
  assert.equal(dark.skin.text.split("\n")[1], "  --fc-color-page-bg: rgb(6 6 12);");
  // glass: a transparent page, translucent bezels
  assert.equal(glass.skin.css.get("color.page.bg"), "rgb(0 0 0 / 0)");
  assert.ok((glass.skin.values.get("color.surface.bezel") as ColorValue).alpha < 0.6);
  assert.deepEqual(glass.skin.values.get("radius.bezel") as Measure, { value: 20, unit: "px" });
});
