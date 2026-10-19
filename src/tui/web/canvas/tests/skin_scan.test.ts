import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { SKIN_SCHEMA } from "../src/bundles/skin/schema.ts";
import { STOCK_SKINS } from "../src/bundles/skin/stock.ts";
import { compileSkin } from "../src/bundles/skin/tokens.ts";
import type { ColorValue } from "../src/bundles/skin/tokens.ts";

// THE SKIN SCAN (phase 4, #49): every colour, radius, font family, shadow and
// motion value of the canvas comes from the skin's tokens — none is written
// into a source. These hold it so.

const APP = fileURLToPath(new URL("..", import.meta.url));
const SRC = join(APP, "src");
/** Content, not style: they keep their colours. */
const CONTENT = new Set(["bundles/backgrounds/stock.ts", "bundles/tips/tips_html.ts", "bundles/html_agent/empty_html.ts"]);
/** The skin's compiler: it writes the colour functions the tokens become. */
const COMPILER = "bundles/skin/tokens.ts";

function sources(): Array<{ rel: string; text: string }> {
  const out: Array<{ rel: string; text: string }> = [];
  const walk = (dir: string): void => {
    for (const n of readdirSync(dir)) {
      const p = join(dir, n);
      const rel = relative(SRC, p).split("\\").join("/");
      if (statSync(p).isDirectory()) {
        if (rel !== "vendor") walk(p);
      } else if (n.endsWith(".ts") && !CONTENT.has(rel)) {
        out.push({ rel, text: readFileSync(p, "utf8") });
      }
    }
  };
  walk(SRC);
  return out;
}

/** The source with its comments blanked (line numbers kept): `/* … *\/`, and
 *  `//` where it starts a comment — `(^|[\s;{}(,])//` keeps `wss://`. */
function stripComments(text: string): string {
  const blank = (s: string): string => s.replace(/[^\n]/g, " ");
  return text.replace(/\/\*[\s\S]*?\*\//g, blank).replace(/(^|[\s;{}(,])\/\/[^\n]*/gm, (m, lead: string) => lead + blank(m.slice(lead.length)));
}

function hits(re: RegExp, check?: (m: RegExpExecArray) => string | null): string[] {
  const out: string[] = [];
  for (const { rel, text } of sources()) {
    const lines = stripComments(text).split("\n");
    lines.forEach((line, i) => {
      re.lastIndex = 0;
      for (let m = re.exec(line); m !== null; m = re.exec(line)) {
        const bad = check === undefined ? m[0] : check(m);
        if (bad !== null) out.push(`${rel}:${i + 1} ${bad}`);
        if (!re.global) break;
      }
    });
  }
  return out;
}

/** A CSS value up to its declaration's end: a `${…}` (tok, tint) is part of it. */
const VALUE = String.raw`((?:\$\{[^}]*\}|[^;"'\`}])*)`;

const NAMED = "white|black|red|green|blue|gray|grey|yellow|orange|purple|pink|silver";

test("no colour literal in the canvas sources", () => {
  const found = [
    ...hits(/#(?:[0-9a-fA-F]{8}|[0-9a-fA-F]{6}|[0-9a-fA-F]{3,4})(?!\w)/g),
    ...hits(/\b(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch|color)\(/g).filter((h) => !h.startsWith(`${COMPILER}:`)),
    ...hits(/\b0x[0-9a-fA-F]{6}(?![0-9a-fA-F])/g),
    // a named colour as a CSS colour value: after a colour property, in a
    // shadow / gradient / border, or set from JS
    ...hits(new RegExp(`(?:color|background|border(?:-[a-z]+)?|outline|fill|stroke|box-shadow|text-shadow|caret-color|gradient\\([^)]*)\\s*[:,]?[^;"'\`]*?\\b(${NAMED})\\b`, "gi"), (m) => {
      // `white-space`, a class name or an identifier is no colour
      const before = m[0].slice(0, m[0].length - m[1]!.length);
      if (/[-\w]$/.test(before) || /^-/.test(m[0].slice(before.length + m[1]!.length))) return null;
      return m[1]!;
    }),
  ];
  assert.deepEqual(found, [], `colour literals left (use tok()/tint() from bundles/skin/css.ts):\n${found.join("\n")}`);
});

test("no font stack, radius or duration literal in a CSS declaration", () => {
  const found = [
    // font-family: / font: — inherit, or a var(--fc-font-…) (built by tok)
    ...hits(new RegExp(String.raw`\bfont(?:-family)?\s*:\s*` + VALUE, "g"), (m) => {
      const v = m[1]!.trim();
      if (v === "" || /^inherit\b/.test(v) || v.includes("${tok(\"font.family.") || v.includes("var(--fc-font-")) return null;
      return `font: ${v}`;
    }),
    ...hits(new RegExp(String.raw`\bborder-radius\s*:\s*` + VALUE, "g"), (m) => {
      const v = m[1]!.trim();
      if (v === "0" || v === "50%" || v === "" || v.includes("${tok(\"radius.") || v.includes("var(--fc-radius-")) {
        // every length in it is a token (or 0 / 50%)
        const rest = v.replace(/\$\{tok\("radius\.[a-z-]+"\)\}/g, "").replace(/var\(--fc-radius-[a-z-]+\)/g, "");
        return /\d+(?:\.\d+)?(?:px|rem|em)/.test(rest) ? `border-radius: ${v}` : null;
      }
      return `border-radius: ${v}`;
    }),
    ...hits(new RegExp(String.raw`\b(?:transition|animation)\s*:\s*` + VALUE, "g"), (m) => (/(?<![\w-])\d*\.?\d+m?s\b/.test(m[1]!) ? m[0] : null)),
    // xterm's font
    ...hits(/\bfontFamily\s*:\s*(["'`])/g),
  ];
  assert.deepEqual(found, [], `literals left (use tok() from bundles/skin/css.ts):\n${found.join("\n")}`);
});

test("every tok() and tint() path is in SKIN_SCHEMA and every tinted path is opaque", async () => {
  // importing every module runs its CSS template strings: a bad path throws, and
  // TINTED fills with every tinted one
  for (const { rel } of sources()) {
    if (rel === "main.ts" || rel === "bundle.ts") continue; // the page's entry: it boots
    // a file:// URL, not a path: Windows' ESM loader takes no `D:\…` path
    await import(pathToFileURL(join(SRC, rel)).href);
  }
  // the same URL the modules above reached it by: one module instance, one TINTED
  const { TINTED } = await import(pathToFileURL(join(SRC, "bundles/skin/css.ts")).href);
  assert.ok(TINTED.size > 0);
  const opaque = new Set(SKIN_SCHEMA.filter((t) => t.opaque === true).map((t) => t.path));
  for (const p of TINTED) assert.ok(opaque.has(p), `${p} is tinted but not opaque in SKIN_SCHEMA`);
  // and every source's tok/tint paths are schema paths (also the ones not run at import)
  const paths = new Set(SKIN_SCHEMA.map((t) => t.path));
  const used = hits(/\b(?:tok|tint)\("([^"]+)"/g, (m) => (paths.has(m[1]!) ? null : m[1]!));
  assert.deepEqual(used, [], "paths not in SKIN_SCHEMA");
  // both stock skins give every opaque token alpha 1
  for (const s of STOCK_SKINS) {
    const c = compileSkin(s.tokens);
    assert.ok(c.ok, JSON.stringify(c));
    if (!c.ok) continue;
    for (const p of opaque) assert.equal((c.skin.values.get(p) as ColorValue).alpha, 1, `${s.name}: ${p}`);
  }
});

test("docs/skins.md lists every schema token", () => {
  const doc = readFileSync(join(APP, "../../docs/skins.md"), "utf8");
  const missing = SKIN_SCHEMA.filter((t) => !doc.includes(`\`${t.path}\``)).map((t) => t.path);
  assert.deepEqual(missing, [], "tokens missing from src/tui/docs/skins.md");
});
