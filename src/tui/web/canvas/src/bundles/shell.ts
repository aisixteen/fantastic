// THE HOST SHELL'S FLAG — a native shell (phase 6, #48: Tauri) says what hosts
// the page with ONE frozen global, set by its initialization script before any
// page script runs:
//
//   window.__FANTASTIC_SHELL__ = Object.freeze({name, os, webview, backdrop, pickFolder?})
//
// A plain browser tab has no flag. Who reads what:
//   backdrop    the skin (phase 4, #49): `native` — the window draws a material
//               behind a transparent page; `web` — the page paints its own
//   webview     the terminal's renderer (phase 7, #52)
//   pickFolder  the Home's folder dialog (phase 3, #47)

export interface FantasticShell {
  name: string; // "tauri"
  os: "macos" | "windows" | "linux";
  webview: "wkwebview" | "webview2" | "webkitgtk";
  backdrop: "native" | "web"; // native: the window draws a material behind a transparent page
  pickFolder?: () => Promise<string[]>; // the shell's folder dialog (read by the Home, phase 3)
}

const OS = ["macos", "windows", "linux"] as const;
const WEBVIEW = ["wkwebview", "webview2", "webkitgtk"] as const;
const BACKDROP = ["native", "web"] as const;

/** null in a plain browser tab; throws Error("__FANTASTIC_SHELL__: <field> must be …") on a malformed flag */
export function readShell(): FantasticShell | null {
  const f = (globalThis as { __FANTASTIC_SHELL__?: unknown }).__FANTASTIC_SHELL__;
  if (f === undefined || f === null) return null;
  const bad = (field: string, what: string): never => {
    throw new Error(`__FANTASTIC_SHELL__: ${field} must be ${what}`);
  };
  if (typeof f !== "object" || Array.isArray(f)) return bad("the flag", "an object");
  const o = f as Record<string, unknown>;
  if (typeof o["name"] !== "string" || o["name"] === "") bad("name", "a non-empty string");
  if (!(OS as readonly unknown[]).includes(o["os"])) bad("os", OS.join(" | "));
  if (!(WEBVIEW as readonly unknown[]).includes(o["webview"])) bad("webview", WEBVIEW.join(" | "));
  if (!(BACKDROP as readonly unknown[]).includes(o["backdrop"])) bad("backdrop", BACKDROP.join(" | "));
  if (o["pickFolder"] !== undefined && typeof o["pickFolder"] !== "function") bad("pickFolder", "a function, when given");
  return f as FantasticShell;
}

export const backdropNow = (): "web" | "native" => readShell()?.backdrop ?? "web";
