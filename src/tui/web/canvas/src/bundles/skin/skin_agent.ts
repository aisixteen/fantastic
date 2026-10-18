import { Agent, str } from "../../lib.ts";
import type { Handler, Json, Kernel, Payload } from "../../lib.ts";
import { backdropNow } from "../shell.ts";
import { SKIN_SCHEMA, cssName } from "./schema.ts";
import { DEFAULT_SKINS, STOCK_SKINS, stockSkin } from "./stock.ts";
import { REFUSED_FEATURES, SUPPORTED_TYPES, compileSkin, mergeTokens } from "./tokens.ts";
import type { Backdrop, Compiled, CompiledSkin } from "./tokens.ts";

// THE SKIN AGENT (phase 4, #49) — the page's skin: which design tokens every
// view is drawn with, and its backdrop. One per canvas (peer canvases too), a
// delete-locked child of the page root, made by the canvas when missing
// (`ensureSkin`). Its STATE is its record, so it survives reloads:
//
//   skins       {web: <name>, native: <name>} — the skin used on each backdrop.
//               The skin IN USE is skins[<this tab's backdrop>]: one canvas can
//               show glass in the app's window and dark in a browser tab at once.
//   user_skins  [{name, base, note, tokens}] — skins written by an AI or a human:
//               `tokens` is a partial token document merged over `base` (a stock
//               skin or another user skin).
//
// The page applies the skin in use (`apply.ts` watchSkin) on every change of
// these two fields — live, without a reload.

export const SKIN = "skin.ts";
export const SKIN_ID = "skin";

/** What an AI cell (or a panel) calls on it. */
export const SKIN_VERBS = ["list", "get", "schema", "use", "add", "delete"] as const;

const USER_NAME = /^[a-z0-9][a-z0-9_-]{0,39}$/;

export const SKIN_README =
  "THE SKIN — how this canvas looks: the design tokens (W3C Design Tokens 2025.10, a subset) every view is drawn with — colours, corner radii, font families, shadows, motion — plus a BACKDROP: `web` (the page paints its own background) or `native` (the app's window draws a material behind a transparent page). " +
  "`skins` on its record names the skin used on each backdrop ({web, native}); the one IN USE is the slot of this tab's backdrop (a browser tab is always web). " +
  "STOCK skins: dark (web — the canvas's own look), glass (native — translucent, for the app's window). A skin whose backdrop is native cannot go in the web slot (nothing is behind a browser tab). " +
  "TO WRITE A SKIN: 1. `schema` names every token (path, type, CSS name, what it paints). 2. `get {name: \"dark\"}` gives a complete example (`merged`). " +
  "3. `add {name, base, tokens, note?}` — `tokens` is a PARTIAL token document merged over `base`: a token you give replaces the base's whole. e.g. " +
  'add {name: "ocean", base: "dark", tokens: {color: {accent: {base: {$value: {colorSpace: "srgb", components: [0.2, 0.7, 0.9]}}}}}}. ' +
  "4. `use {name: \"ocean\"}` — the page shows it at once. A bad token is refused naming its path. Colours are srgb {colorSpace, components: [r, g, b] in [0, 1], alpha?, hex? (6-digit, must match the components)}; tokens marked opaque take alpha 1. " +
  "`palette.*` is a free group of alias targets (`{palette.sea}`), never compiled. `delete {name}` removes one of yours (not while in use, nor while another skin's base).";

type Dict = Record<string, unknown>;
const isDict = (v: unknown): v is Dict => typeof v === "object" && v !== null && !Array.isArray(v);

export interface UserSkin {
  name: string;
  base: string;
  note: string;
  tokens: Dict;
}

export interface Slots {
  web: string;
  native: string;
}

/** The record's slots, defaulted. */
export function slotsOf(meta: Dict): Slots {
  const s = isDict(meta["skins"]) ? meta["skins"] : {};
  return {
    web: typeof s["web"] === "string" && s["web"] !== "" ? s["web"] : DEFAULT_SKINS.web,
    native: typeof s["native"] === "string" && s["native"] !== "" ? s["native"] : DEFAULT_SKINS.native,
  };
}

/** The record's user skins (malformed entries dropped). */
export function userSkinsOf(meta: Dict): UserSkin[] {
  const a = Array.isArray(meta["user_skins"]) ? (meta["user_skins"] as unknown[]) : [];
  return a
    .filter((u): u is Dict => isDict(u) && typeof u["name"] === "string" && typeof u["base"] === "string")
    .map((u) => ({ name: u["name"] as string, base: u["base"] as string, note: typeof u["note"] === "string" ? u["note"] : "", tokens: isDict(u["tokens"]) ? u["tokens"] : {} }));
}

export type Resolved =
  | { ok: true; name: string; from: "stock" | "user"; base?: string; note: string; tokens: Dict; merged: Dict }
  | { ok: false; error: string; reason: string; arg?: string; path?: string };

/** A skin by name: stock, or a user skin merged over its base chain. */
export function resolveSkin(user: UserSkin[], name: string): Resolved {
  const chain: string[] = [];
  const walk = (n: string): Resolved => {
    const s = stockSkin(n);
    if (s !== undefined) return { ok: true, name: n, from: "stock", note: s.note, tokens: s.tokens, merged: mergeTokens(s.tokens, {}) };
    const u = user.find((x) => x.name === n);
    if (u === undefined) {
      return chain.length === 0
        ? { ok: false, error: `skin: no skin ${JSON.stringify(n)}`, reason: "not_found", arg: "name" }
        : { ok: false, error: `skin: ${chain[chain.length - 1]}'s base ${JSON.stringify(n)} is no skin`, reason: "not_found", arg: "base" };
    }
    if (chain.includes(n)) return { ok: false, error: `skin: a base cycle: ${[...chain, n].join(" → ")}`, reason: "bad_arg", arg: "base" };
    chain.push(n);
    const b = walk(u.base);
    if (!b.ok) return b;
    return { ok: true, name: n, from: "user", base: u.base, note: u.note, tokens: u.tokens, merged: mergeTokens(b.merged, u.tokens) };
  };
  return walk(name);
}

/** A skin by name, compiled. */
export function compileNamed(user: UserSkin[], name: string): { ok: true; resolved: Extract<Resolved, { ok: true }>; skin: CompiledSkin } | { ok: false; error: string; reason: string; arg?: string; path?: string } {
  const r = resolveSkin(user, name);
  if (!r.ok) return r;
  const c: Compiled = compileSkin(r.merged);
  if (!c.ok) return { ok: false, error: c.error, reason: c.reason, arg: c.arg, path: c.path };
  return { ok: true, resolved: r, skin: c.skin };
}

const refusal = (e: { error: string; reason: string; arg?: string; path?: string }): Json => {
  const out: Dict = { error: e.error, reason: e.reason };
  if (e.arg !== undefined) out["arg"] = e.arg;
  if (e.path !== undefined) out["path"] = e.path;
  return out as Json;
};

/** What `schema` answers. */
export function schemaAnswer(): Json {
  return {
    format: "W3C Design Tokens 2025.10 (subset)",
    supported_types: [...SUPPORTED_TYPES],
    refused: [...REFUSED_FEATURES, "colorSpace other than srgb", "the none colour keyword"],
    extension: "fantastic",
    backdrop: '$extensions.fantastic.backdrop: "web" | "native" on the document root (a user skin keeps its base\'s unless it sets its own)',
    tokens: SKIN_SCHEMA.map((t) => ({ path: t.path, type: t.type, opaque: t.opaque === true, note: t.note, css: cssName(t.path) })),
    palette: "palette.* — free alias targets, never compiled",
  } as unknown as Json;
}

export const skinAgent: Handler = (id, payload, kernel): Json => {
  const me = kernel.get(id);
  if (me === undefined) return { error: `skin: no agent ${id}`, reason: "not_found" };
  const meta = me.meta as Dict;
  const user = userSkinsOf(meta);
  const slots = slotsOf(meta);
  const here = backdropNow();
  const backdropOf = (name: string): Backdrop | null => {
    const c = compileNamed(user, name);
    return c.ok ? c.skin.backdrop : null;
  };
  switch (str(payload, "type")) {
    case "list":
      return {
        stock: STOCK_SKINS.map((s) => ({ name: s.name, note: s.note, backdrop: backdropOf(s.name) })),
        user: user.map((u) => {
          const c = compileNamed(user, u.name);
          return c.ok ? { name: u.name, base: u.base, note: u.note, backdrop: c.skin.backdrop } : { name: u.name, base: u.base, note: u.note, backdrop: null, error: c.error };
        }),
        skins: { ...slots },
        backdrop: here,
        in_use: slots[here],
      } as unknown as Json;
    case "get": {
      const name = str(payload, "name");
      const c = compileNamed(user, name);
      if (!c.ok) {
        // a user skin that no longer compiles is still shown, with its error
        const r = resolveSkin(user, name);
        if (!r.ok) return refusal(c);
        return { name, from: r.from, ...(r.base !== undefined ? { base: r.base } : {}), note: r.note, backdrop: null, tokens: r.tokens, merged: r.merged, error: c.error } as unknown as Json;
      }
      const r = c.resolved;
      return { name, from: r.from, ...(r.base !== undefined ? { base: r.base } : {}), note: r.note, backdrop: c.skin.backdrop, tokens: r.tokens, merged: r.merged } as unknown as Json;
    }
    case "schema":
      return schemaAnswer();
    case "use": {
      const name = str(payload, "name");
      const b = payload["backdrop"];
      if (b !== undefined && b !== "web" && b !== "native") return { error: 'skin: use — backdrop is "web" or "native"', reason: "bad_arg", arg: "backdrop" };
      const slot: Backdrop = (b as Backdrop | undefined) ?? here;
      const c = compileNamed(user, name);
      if (!c.ok) return { ...(refusal(c) as Dict), hint: "`list` names the skins" } as Json;
      if (slot === "web" && c.skin.backdrop === "native") {
        return { error: `skin: ${name} is a native-backdrop skin — it cannot go in the web slot (nothing is behind a browser tab)`, reason: "bad_arg", arg: "name", hint: "use a web skin here, or {backdrop: \"native\"} for the app's window" };
      }
      const next = { ...slots, [slot]: name };
      kernel.updateMeta(id, { skins: next } as unknown as Payload);
      return { skins: next, in_use: next[here] } as unknown as Json;
    }
    case "add": {
      const name = str(payload, "name").trim();
      if (!USER_NAME.test(name)) return { error: "skin: add — name is 1–40 of a-z 0-9 _ -, starting with a letter or digit", reason: "bad_arg", arg: "name" };
      if (stockSkin(name) !== undefined) return { error: `skin: add — ${name} is a stock skin; give yours another name`, reason: "bad_arg", arg: "name" };
      const base = str(payload, "base").trim();
      if (base === "") return { error: "skin: add needs base: the stock skin (dark, glass) or user skin it is merged over", reason: "bad_arg", arg: "base" };
      const tokens = payload["tokens"];
      if (!isDict(tokens)) return { error: "skin: add needs tokens: a (partial) token document, merged over base", reason: "bad_arg", arg: "tokens" };
      const note = str(payload, "note");
      const entry: UserSkin = { name, base, note, tokens: tokens as Dict };
      const next = [...user.filter((u) => u.name !== name), entry];
      const c = compileNamed(next, name);
      if (!c.ok) return refusal(c);
      // replacing a skin in use: its new version must still fit its slot
      if (slots.web === name && c.skin.backdrop === "native") return { error: `skin: ${name} is in the web slot — a native-backdrop version cannot replace it there`, reason: "bad_arg", arg: "tokens", hint: "use another web skin first" };
      kernel.updateMeta(id, { user_skins: next } as unknown as Payload);
      return { added: name, base, backdrop: c.skin.backdrop };
    }
    case "delete": {
      const name = str(payload, "name");
      if (stockSkin(name) !== undefined) return { error: `skin: ${name} is a stock skin — it cannot be deleted`, reason: "bad_arg", arg: "name" };
      if (!user.some((u) => u.name === name)) return { error: `skin: no skin ${JSON.stringify(name)} of yours`, reason: "not_found", arg: "name" };
      const slot = (["web", "native"] as const).find((s) => slots[s] === name);
      if (slot !== undefined) return { error: `skin: ${name} is in use (the ${slot} slot) — use another first`, reason: "bad_arg", arg: "name" };
      const child = user.find((u) => u.base === name);
      if (child !== undefined) return { error: `skin: ${name} is ${child.name}'s base — delete or rebase ${child.name} first`, reason: "bad_arg", arg: "name" };
      kernel.updateMeta(id, { user_skins: user.filter((u) => u.name !== name) } as unknown as Payload);
      return { deleted: name };
    }
    case "boot":
    case "shutdown":
      return null;
    default:
      return {
        error: `skin: unknown verb '${str(payload, "type")}'`,
        reason: "unhandled",
        verb: str(payload, "type"),
        hint: `it answers ${SKIN_VERBS.join(", ")}`,
      };
  }
};

/** The page's skin agent, made once when missing — a child of the page root. */
export function ensureSkin(kernel: Kernel): void {
  if (kernel.get(SKIN_ID) !== undefined || kernel.rootId === null) return;
  kernel.register(
    new Agent({
      id: SKIN_ID,
      parentId: kernel.rootId,
      handlerModule: SKIN,
      sentence: "The page's skin: the design tokens every view is drawn with, and its backdrop.",
      meta: { delete_lock: true, skins: { ...DEFAULT_SKINS }, user_skins: [], callable_by: { $self: "*" } } as unknown as Payload,
    }),
  );
}

export function registerSkin(kernel: {
  registerBundle(hm: string, h: Handler, verbs?: Record<string, string>): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(SKIN, skinAgent, {
    list: "No args. {stock: [{name, note, backdrop}], user: [{name, base, note, backdrop, error?}], skins: {web, native}, backdrop: this tab's, in_use}.",
    get: "args: name:str. {name, from: stock|user, base?, note, backdrop, tokens (as authored), merged (the full document that compiles)}.",
    schema: "No args. The token format and every token the canvas reads: {format, supported_types, refused, extension, tokens: [{path, type, opaque, note, css}], palette}.",
    use: "args: name:str, backdrop?:web|native (default: this tab's). Sets that slot's skin; the page shows it at once. A native skin is refused for the web slot.",
    add: "args: name:str, base:str (a stock or user skin), tokens:dict (a partial token document merged over base), note?:str. Adds or replaces one of YOURS; a bad token is refused naming its path. {added, base, backdrop}.",
    delete: "args: name:str. Deletes one of yours — not while it is in a slot, nor while it is another skin's base. {deleted}.",
  });
  kernel.setBundleReadme(SKIN, SKIN_README);
}
