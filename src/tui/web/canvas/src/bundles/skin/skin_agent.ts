import { Agent, str } from "../../lib.ts";
import type { Handler, Json, Kernel, Payload } from "../../lib.ts";
import { SKIN_SCHEMA, cssName } from "./schema.ts";
import { stockSkin } from "./stock.ts";
import { REFUSED_FEATURES, SUPPORTED_TYPES, compileSkin } from "./tokens.ts";
import type { CompiledSkin } from "./tokens.ts";

// THE SKIN AGENT (phase 4, #49) — the page's skin: the design tokens every view
// is drawn with, and its backdrop. One per canvas (peer canvases too), a
// delete-locked child of the page root, made by the canvas when missing
// (`ensureSkin`). It is read only: the skin is fixed by the backdrop — day on
// the web, glass in the app's window (`DEFAULT_SKINS`) — and applied once by the
// page (`apply.ts`); nothing on its record chooses it.

export const SKIN = "skin.ts";
export const SKIN_ID = "skin";

/** What an AI cell (or a panel) calls on it. */
export const SKIN_VERBS = ["get", "schema"] as const;

export const SKIN_README =
  "THE SKIN — how this canvas looks: the design tokens (W3C Design Tokens 2025.10, a subset) every view is drawn with — colours, corner radii, font families, shadows, motion — plus a BACKDROP: `web` (the page paints its own background) or `native` (the app's window draws a material behind a transparent page). " +
  "The skin is fixed by the backdrop: `day` on the web (black and white glass), `glass` in the app's window (translucent). It is read only here. " +
  "`get {name: \"day\"}` → the skin's tokens as authored and its backdrop; `schema` names every token (path, type, CSS name, what it paints) and the token format.";

type Dict = Record<string, unknown>;

/** A stock skin by name, compiled. */
export function compileNamed(name: string): { ok: true; skin: CompiledSkin } | { ok: false; error: string; reason: string; arg?: string; path?: string } {
  const s = stockSkin(name);
  if (s === undefined) return { ok: false, error: `skin: no skin ${JSON.stringify(name)}`, reason: "not_found", arg: "name" };
  const c = compileSkin(s.tokens);
  if (!c.ok) return { ok: false, error: c.error, reason: c.reason, arg: c.arg, path: c.path };
  return { ok: true, skin: c.skin };
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
    backdrop: '$extensions.fantastic.backdrop: "web" | "native" on the document root',
    tokens: SKIN_SCHEMA.map((t) => ({ path: t.path, type: t.type, opaque: t.opaque === true, note: t.note, css: cssName(t.path) })),
    palette: "palette.* — free alias targets, never compiled",
  } as unknown as Json;
}

export const skinAgent: Handler = (id, payload, kernel): Json => {
  if (kernel.get(id) === undefined) return { error: `skin: no agent ${id}`, reason: "not_found" };
  switch (str(payload, "type")) {
    case "get": {
      const name = str(payload, "name");
      const c = compileNamed(name);
      if (!c.ok) return refusal(c);
      const s = stockSkin(name)!;
      return { name, note: s.note, backdrop: c.skin.backdrop, tokens: s.tokens } as unknown as Json;
    }
    case "schema":
      return schemaAnswer();
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
      meta: { delete_lock: true, callable_by: { $self: "*" } } as unknown as Payload,
    }),
  );
}

export function registerSkin(kernel: {
  registerBundle(hm: string, h: Handler, verbs?: Record<string, string>): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(SKIN, skinAgent, {
    get: "args: name:str. {name, note, backdrop, tokens (as authored)}.",
    schema: "No args. The token format and every token the canvas reads: {format, supported_types, refused, extension, tokens: [{path, type, opaque, note, css}], palette}.",
  });
  kernel.setBundleReadme(SKIN, SKIN_README);
}
