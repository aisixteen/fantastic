import { SKIN_SCHEMA, cssName } from "./schema.ts";
import type { SchemaToken, TokenType } from "./schema.ts";

// A SKIN'S TOKENS — the reader and compiler of a W3C Design Tokens Format
// Module 2025.10 document (https://www.designtokens.org/tr/2025.10/format/), a
// STRICT SUBSET of it: what this canvas does not support is refused by name,
// never silently ignored. Pure: no DOM, so node's tests run it as the page does.
//
//   parseTokens(doc)        → every token, its type and its resolved value
//   mergeTokens(base, over) → a partial skin over its base (a token replaced whole)
//   compileSkin(doc)        → the skin the page installs: its CSS custom
//                             properties, its resolved values, its backdrop
//
// Every refusal is the kernel's shape: {error: "skin: <path> — <why>",
// reason: "bad_arg", arg: "tokens", path}.

export type Backdrop = "web" | "native";

export interface ColorValue {
  /** 6-digit `#rrggbb` */
  hex: string;
  alpha: number;
  /** its CSS text: `rgb(R G B)` or `rgb(R G B / A)` */
  css: string;
  components: [number, number, number];
}
export interface Measure {
  value: number;
  unit: string;
}
export interface ShadowLayer {
  color: ColorValue;
  offsetX: Measure;
  offsetY: Measure;
  blur: Measure;
  spread: Measure;
  inset: boolean;
}
export type ResolvedValue = ColorValue | Measure | number[] | string[] | number | ShadowLayer[];

export interface Token {
  type: TokenType;
  value: ResolvedValue;
}

export interface CompiledSkin {
  backdrop: Backdrop;
  /** token path → its CSS text, in SKIN_SCHEMA order */
  css: ReadonlyMap<string, string>;
  /** token path → resolved value for JS consumers (colours: {hex: "#rrggbb", alpha, css}) */
  values: ReadonlyMap<string, ResolvedValue>;
  /** the stylesheet: ":root {\n  --fc-color-page-bg: rgb(6 6 12);\n  …\n}\n" */
  text: string;
}

export interface SkinError {
  ok: false;
  error: string;
  reason: "bad_arg";
  arg: "tokens";
  path: string;
}
export type Parsed = { ok: true; tokens: Map<string, Token>; backdrop: Backdrop } | SkinError;
export type Compiled = { ok: true; skin: CompiledSkin } | SkinError;

/** The DTCG types this canvas reads. */
export const SUPPORTED_TYPES: ReadonlyArray<TokenType> = ["color", "dimension", "duration", "cubicBezier", "fontFamily", "fontWeight", "number", "shadow"];
/** The DTCG types it refuses, by name. */
export const REFUSED_TYPES = ["strokeStyle", "border", "transition", "gradient", "typography"] as const;
/** The DTCG features it refuses, by name. */
export const REFUSED_FEATURES = ["$ref (JSON Pointer references)", "$extends (group inheritance)", "$root (a group's root token)", ...REFUSED_TYPES.map((t) => `$type ${t}`)] as const;

/** The spec's fontWeight aliases (Format §8.4), mapped to their numbers. */
const FONT_WEIGHTS: Readonly<Record<string, number>> = {
  thin: 100, hairline: 100,
  "extra-light": 200, "ultra-light": 200,
  light: 300,
  normal: 400, regular: 400, book: 400,
  medium: 500,
  "semi-bold": 600, "demi-bold": 600,
  bold: 700,
  "extra-bold": 800, "ultra-bold": 800,
  black: 900, heavy: 900,
  "extra-black": 950, "ultra-black": 950,
};

/** A name this schema accepts: it becomes part of a CSS property name. */
const NAME = /^[a-z0-9]+(-[a-z0-9]+)*$/;
const ALIAS = /^\{([^{}]+)\}$/;
const HEX6 = /^#[0-9a-fA-F]{6}$/;
const TOKEN_KEYS = new Set(["$value", "$type", "$description", "$extensions", "$deprecated"]);
const GROUP_KEYS = new Set(["$type", "$description", "$extensions", "$deprecated"]);
const REFUSED_KEYS: Readonly<Record<string, string>> = {
  $ref: "$ref (JSON Pointer references) is not supported in this canvas — use a curly-brace alias",
  $extends: "$extends (group inheritance) is not supported in this canvas — merge over a base skin instead",
  $root: "$root (a group's root token) is not supported in this canvas",
};

type Dict = Record<string, unknown>;
const isDict = (v: unknown): v is Dict => typeof v === "object" && v !== null && !Array.isArray(v);

class Refusal extends Error {
  readonly path: string;
  readonly why: string;
  constructor(path: string, why: string) {
    super(why);
    this.path = path;
    this.why = why;
  }
}
const refuse = (path: string, why: string): never => {
  throw new Refusal(path, why);
};
const asError = (e: unknown): SkinError => {
  if (e instanceof Refusal) {
    const where = e.path === "" ? "(root)" : e.path;
    return { ok: false, error: `skin: ${where} — ${e.why}`, reason: "bad_arg", arg: "tokens", path: where };
  }
  throw e;
};

/** A raw token, before its aliases resolve. */
interface Raw {
  type: TokenType | null;
  value: unknown;
}

// ─── parse ────────────────────────────────────────────────────────────────

/** Read a token document: its groups, tokens, inherited `$type`s and aliases,
 *  every value checked against its type. */
export function parseTokens(doc: unknown): Parsed {
  try {
    return parse(doc);
  } catch (e) {
    return asError(e);
  }
}

function parse(doc: unknown): { ok: true; tokens: Map<string, Token>; backdrop: Backdrop } {
  if (!isDict(doc)) refuse("", "a token document is a JSON object");
  const root = doc as Dict;
  const backdrop = backdropOf(root);
  const raws = new Map<string, Raw>();
  const groups = new Set<string>([""]);
  walk(root, "", null, raws, groups);
  const tokens = new Map<string, Token>();
  const resolving: string[] = [];
  const resolve = (path: string): Token => {
    const done = tokens.get(path);
    if (done !== undefined) return done;
    const raw = raws.get(path)!;
    const at = resolving.indexOf(path);
    if (at >= 0) refuse(path, `an alias cycle: ${[...resolving.slice(at), path].join(" → ")}`);
    resolving.push(path);
    let tok: Token;
    const alias = aliasOf(raw.value);
    if (alias !== null) {
      const target = targetOf(path, alias, raws, groups);
      const t = resolve(target);
      if (raw.type !== null && raw.type !== t.type) refuse(path, `its $type is ${raw.type}, but its alias {${target}} is a ${t.type}`);
      tok = t;
    } else {
      if (raw.type === null) refuse(path, "a token needs a $type (its own, or inherited from a group)");
      tok = { type: raw.type!, value: valueOf(path, raw.type!, raw.value, (a) => resolve(targetOf(path, a, raws, groups))) };
    }
    resolving.pop();
    tokens.set(path, tok);
    return tok;
  };
  for (const path of raws.keys()) resolve(path);
  return { ok: true, tokens, backdrop };
}

function backdropOf(root: Dict): Backdrop {
  const ext = root["$extensions"];
  const f = isDict(ext) ? ext["fantastic"] : undefined;
  if (!isDict(f)) refuse("", 'the document needs "$extensions": {"fantastic": {"backdrop": "web" | "native"}}');
  const b = (f as Dict)["backdrop"];
  if (b !== "web" && b !== "native") refuse("$extensions.fantastic.backdrop", 'backdrop is "web" or "native"');
  return b as Backdrop;
}

function checkType(path: string, t: unknown): TokenType {
  if (typeof t !== "string") return refuse(path, "$type is a string");
  if ((REFUSED_TYPES as readonly string[]).includes(t)) return refuse(path, `$type ${t} is not supported in this canvas (supported: ${SUPPORTED_TYPES.join(", ")})`);
  if (!(SUPPORTED_TYPES as readonly string[]).includes(t)) return refuse(path, `unknown $type ${JSON.stringify(t)} (supported: ${SUPPORTED_TYPES.join(", ")})`);
  return t as TokenType;
}

function walk(node: Dict, path: string, inherited: TokenType | null, raws: Map<string, Raw>, groups: Set<string>): void {
  for (const k of Object.keys(node)) if (REFUSED_KEYS[k] !== undefined) refuse(path, REFUSED_KEYS[k]!);
  const own = node["$type"] !== undefined ? checkType(path, node["$type"]) : null;
  if ("$value" in node) {
    const children = Object.keys(node).filter((k) => !k.startsWith("$"));
    if (children.length > 0) refuse(path, `a token holds $value and so cannot hold children (${children.join(", ")})`);
    for (const k of Object.keys(node)) if (!TOKEN_KEYS.has(k)) refuse(path, `unknown token property ${k}`);
    if (path === "") refuse("", "the document root is a group, not a token");
    raws.set(path, { type: own ?? inherited, value: node["$value"] });
    return;
  }
  const inPalette = path === "palette" || path.startsWith("palette.");
  for (const [k, v] of Object.entries(node)) {
    if (k.startsWith("$")) {
      if (!GROUP_KEYS.has(k)) refuse(path, `unknown group property ${k}`);
      continue;
    }
    const child = path === "" ? k : `${path}.${k}`;
    if (/[{}.]/.test(k)) refuse(child, "a name must not contain { } or .");
    if (!inPalette && !NAME.test(k)) refuse(child, "a name is lowercase letters and digits, words joined by - (it becomes a CSS property name)");
    if (!isDict(v)) refuse(child, "is neither a token (an object with $value) nor a group (an object)");
    groups.add(child);
    walk(v as Dict, child, own ?? inherited, raws, groups);
    if (raws.has(child)) groups.delete(child);
  }
}

const aliasOf = (v: unknown): string | null => {
  if (typeof v !== "string") return null;
  const m = ALIAS.exec(v);
  return m === null ? null : m[1]!;
};

function targetOf(path: string, target: string, raws: Map<string, Raw>, groups: Set<string>): string {
  if (raws.has(target)) return target;
  if (groups.has(target)) return refuse(path, `the alias {${target}} names a group, not a token`);
  return refuse(path, `the alias {${target}} names no token`);
}

// ─── values ───────────────────────────────────────────────────────────────

const num = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
const in01 = (v: unknown): v is number => num(v) && v >= 0 && v <= 1;
const hex2 = (n: number): string => n.toString(16).padStart(2, "0");

function valueOf(path: string, type: TokenType, v: unknown, alias: (target: string) => Token): ResolvedValue {
  switch (type) {
    case "color":
      return colorOf(path, v);
    case "dimension":
      return measureOf(path, v, ["px", "rem"], "dimension");
    case "duration":
      return measureOf(path, v, ["ms", "s"], "duration");
    case "cubicBezier": {
      if (!Array.isArray(v) || v.length !== 4 || !v.every(num)) return refuse(path, "a cubicBezier is four numbers [x1, y1, x2, y2]");
      if (!in01(v[0]) || !in01(v[2])) return refuse(path, "a cubicBezier's x1 and x2 are in [0, 1]");
      return [...(v as number[])];
    }
    case "fontFamily": {
      if (typeof v === "string" && v.trim() !== "") return [v];
      if (Array.isArray(v) && v.length > 0 && v.every((x) => typeof x === "string" && x.trim() !== "")) return [...(v as string[])];
      return refuse(path, "a fontFamily is a name or a non-empty array of names");
    }
    case "fontWeight": {
      if (typeof v === "string" && FONT_WEIGHTS[v] !== undefined) return FONT_WEIGHTS[v]!;
      if (num(v) && v >= 1 && v <= 1000) return v;
      return refuse(path, `a fontWeight is a number in [1, 1000] or one of ${Object.keys(FONT_WEIGHTS).join(", ")}`);
    }
    case "number":
      if (!num(v)) return refuse(path, "a number token's $value is a JSON number");
      return v;
    case "shadow": {
      const layers = Array.isArray(v) ? v : [v];
      if (layers.length === 0) return refuse(path, "a shadow is an object or a non-empty array of objects");
      return layers.map((l, i) => shadowOf(layers.length > 1 ? `${path}[${i}]` : path, l, alias));
    }
  }
}

function colorOf(path: string, v: unknown): ColorValue {
  if (v === "none") return refuse(path, "the none keyword is not supported in this canvas — give alpha 0");
  if (!isDict(v)) return refuse(path, 'a color is {colorSpace: "srgb", components: [r, g, b], alpha?, hex?}');
  if (v["colorSpace"] !== "srgb") return refuse(path, `colorSpace ${JSON.stringify(v["colorSpace"])} — only srgb in this canvas`);
  const c = v["components"];
  if (!Array.isArray(c) || c.length !== 3 || !c.every(in01)) return refuse(path, "components must be three numbers in [0, 1]");
  const alpha = v["alpha"] === undefined ? 1 : v["alpha"];
  if (!in01(alpha)) return refuse(path, "alpha must be a number in [0, 1]");
  const bytes = (c as number[]).map((x) => Math.round(x * 255));
  const hex = `#${bytes.map(hex2).join("")}`;
  if (v["hex"] !== undefined) {
    if (typeof v["hex"] !== "string" || !HEX6.test(v["hex"])) return refuse(path, "hex, when given, is 6-digit CSS hex (#rrggbb)");
    if (v["hex"].toLowerCase() !== hex) return refuse(path, `hex ${v["hex"]} does not match the components (${hex})`);
  }
  for (const k of Object.keys(v)) if (!["colorSpace", "components", "alpha", "hex"].includes(k)) return refuse(path, `unknown color member ${k}`);
  const css = alpha === 1 ? `rgb(${bytes.join(" ")})` : `rgb(${bytes.join(" ")} / ${alpha})`;
  return { hex, alpha, css, components: [...(c as number[])] as [number, number, number] };
}

function measureOf(path: string, v: unknown, units: string[], what: string): Measure {
  if (!isDict(v) || !num(v["value"]) || typeof v["unit"] !== "string") return refuse(path, `a ${what} is {value: number, unit: ${units.join(" | ")}}`);
  if (!units.includes(v["unit"])) return refuse(path, `a ${what}'s unit is ${units.join(" or ")}, not ${JSON.stringify(v["unit"])}`);
  return { value: v["value"], unit: v["unit"] };
}

function shadowOf(path: string, v: unknown, alias: (target: string) => Token): ShadowLayer {
  if (!isDict(v)) return refuse(path, "a shadow layer is {color, offsetX, offsetY, blur, spread, inset?}");
  // a member may be an alias of its type
  const member = <T>(k: string, type: TokenType, read: (p: string, x: unknown) => T): T => {
    const p = `${path}.${k}`;
    if (v[k] === undefined) return refuse(p, `a shadow layer needs ${k}`);
    const a = aliasOf(v[k]);
    if (a === null) return read(p, v[k]);
    const t = alias(a);
    if (t.type !== type) return refuse(p, `the alias {${a}} is a ${t.type}, not a ${type}`);
    return t.value as T;
  };
  const dim = (p: string, x: unknown): Measure => measureOf(p, x, ["px", "rem"], "dimension");
  if (v["inset"] !== undefined && typeof v["inset"] !== "boolean") return refuse(`${path}.inset`, "inset is a boolean");
  for (const k of Object.keys(v)) if (!["color", "offsetX", "offsetY", "blur", "spread", "inset"].includes(k)) return refuse(path, `unknown shadow member ${k}`);
  return {
    color: member("color", "color", colorOf),
    offsetX: member("offsetX", "dimension", dim),
    offsetY: member("offsetY", "dimension", dim),
    blur: member("blur", "dimension", dim),
    spread: member("spread", "dimension", dim),
    inset: v["inset"] === true,
  };
}

// ─── merge ────────────────────────────────────────────────────────────────

const clone = <T>(v: T): T => JSON.parse(JSON.stringify(v)) as T;

/** A partial skin `over` its `base`: groups merge deeply, a token in `over`
 *  replaces the base's whole, and the root's `$extensions.fantastic` merges the
 *  same way (a skin keeps its base's backdrop unless it sets its own). */
export function mergeTokens(base: unknown, over: unknown): Dict {
  const out = isDict(base) ? clone(base) : {};
  if (!isDict(over)) return out;
  mergeInto(out, over);
  return out;
}

function mergeInto(into: Dict, over: Dict): void {
  for (const [k, v] of Object.entries(over)) {
    const had = into[k];
    const token = isDict(v) && "$value" in v;
    if (!token && isDict(v) && isDict(had) && !("$value" in had)) mergeInto(had, v);
    else into[k] = clone(v);
  }
}

// ─── compile ──────────────────────────────────────────────────────────────

/** A font name, quoted only when it holds a character outside [A-Za-z0-9-]
 *  (WebKit honours `-apple-system` only unquoted). */
const fontName = (n: string): string => (/^[A-Za-z0-9-]+$/.test(n) ? n : `"${n.replace(/(["\\])/g, "\\$1")}"`);
const measure = (m: Measure): string => `${m.value}${m.unit}`;

/** A resolved value's CSS text. */
export function cssOf(t: Token): string {
  switch (t.type) {
    case "color":
      return (t.value as ColorValue).css;
    case "dimension":
    case "duration":
      return measure(t.value as Measure);
    case "cubicBezier":
      return `cubic-bezier(${(t.value as number[]).join(", ")})`;
    case "fontFamily":
      return (t.value as string[]).map(fontName).join(", ");
    case "fontWeight":
    case "number":
      return String(t.value);
    case "shadow":
      return (t.value as ShadowLayer[])
        .map((l) => `${l.inset ? "inset " : ""}${measure(l.offsetX)} ${measure(l.offsetY)} ${measure(l.blur)} ${measure(l.spread)} ${l.color.css}`)
        .join(", ");
  }
}

/** Compile a token document into the skin the page installs, checked against
 *  the schema the canvas reads. */
export function compileSkin(doc: unknown, schema: ReadonlyArray<SchemaToken> = SKIN_SCHEMA): Compiled {
  const parsed = parseTokens(doc);
  if (!parsed.ok) return parsed;
  try {
    const { tokens, backdrop } = parsed;
    // two paths, one CSS name
    const names = new Map<string, string>();
    for (const path of tokens.keys()) {
      if (path === "palette" || path.startsWith("palette.")) continue;
      const n = cssName(path);
      const was = names.get(n);
      if (was !== undefined) refuse(path, `${path} and ${was} are both ${n}`);
      names.set(n, path);
    }
    const known = new Set(schema.map((t) => t.path));
    for (const path of tokens.keys()) {
      if (path === "palette" || path.startsWith("palette.")) continue;
      if (!known.has(path)) refuse(path, `not a token this canvas reads: ${path} (palette.* holds free alias targets)`);
    }
    const css = new Map<string, string>();
    const values = new Map<string, ResolvedValue>();
    for (const s of schema) {
      const t = tokens.get(s.path);
      if (t === undefined) refuse(s.path, `missing: the canvas reads ${s.path} (a ${s.type})`);
      if (t!.type !== s.type) refuse(s.path, `is a ${t!.type}; the canvas reads it as a ${s.type}`);
      if (s.opaque === true && (t!.value as ColorValue).alpha !== 1) refuse(s.path, "is mixed with tint(), so its alpha must be 1");
      css.set(s.path, cssOf(t!));
      values.set(s.path, t!.value);
    }
    const text = `:root {\n${[...css].map(([p, v]) => `  ${cssName(p)}: ${v};\n`).join("")}}\n`;
    return { ok: true, skin: { backdrop, css, values, text } };
  } catch (e) {
    return asError(e);
  }
}

/** A colour as xterm takes it: `#rrggbb` at alpha 1, else `#rrggbbaa`. */
export const xtermColor = (c: ColorValue): string => (c.alpha === 1 ? c.hex : `${c.hex}${hex2(Math.round(c.alpha * 255))}`);
