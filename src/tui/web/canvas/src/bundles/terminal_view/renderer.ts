// THE TERMINAL RENDERER — its own piece, chosen by the VIEW's
// `terminal_renderer: auto | webgl | dom` (phase 7, #52; absent = auto):
//
//   webgl  xterm's WebGL renderer through the page's ONE shared WebGL context —
//          the patched `@xterm/addon-webgl` (`vendor-src/addon-webgl`,
//          provenance in `vendor/VENDOR.md`). Measured (E65 load, headful GPU):
//          xterm's DOM renderer re-rasterized every line's text and fell to
//          ~5 fps at 40 streaming terminals on screen; this holds 60 fps at 200.
//   dom    xterm's own DomRenderer, what a terminal draws with before any addon
//          (the addon's dispose restores it). For WebKitGTK: its WebGL2 context
//          is created even when it is backed by a software rasterizer or a slow
//          presentation path, and it masks the renderer string — the context
//          cannot tell, so WEBKITGTK IS KNOWN BY THE WEBVIEW, never by probing GL.
//   auto   dom in WebKitGTK (the shell's flag says so, else the user agent),
//          webgl everywhere else — resolved in each tab, like the view's theme.
//
// It knows nothing of the canvas: it takes a terminal and the element it lives
// in. A lost context: the addon is dropped and a fresh one made while webgl is
// still wanted. No WebGL2: said loud, in the terminal and the console — xterm
// keeps drawing with the DOM renderer.

import type { FantasticShell } from "../shell.ts";

export type TerminalRenderer = "webgl" | "dom";
export const RENDERER_CHOICES = ["auto", "webgl", "dom"] as const;

/** WebKit on Linux that is not Chromium, Firefox or Android: WebKitGTK (or WPE). */
export function isWebKitGtkUa(ua: string): boolean {
  return /\bLinux\b/.test(ua) && /\bAppleWebKit\//.test(ua) && !/\b(Chrome|Chromium|CriOS|Edg|OPR|Firefox|FxiOS|Android)\b/.test(ua);
}

/** The renderer a view's choice means in this tab, and why. Throws on a value
 *  that is not auto, webgl or dom (a hand-edited record). */
export function resolveRenderer(choice: unknown, env: { shell: FantasticShell | null; ua: string }): { in_use: TerminalRenderer; why: string } {
  if (choice === "webgl" || choice === "dom") return { in_use: choice, why: `set: ${choice}` };
  if (choice !== "auto") throw new Error(`terminal_renderer: ${typeof choice === "string" ? choice : JSON.stringify(choice)} is not auto, webgl or dom`);
  // a shell knows its webview; a user agent can be customized
  if (env.shell) return { in_use: env.shell.webview === "webkitgtk" ? "dom" : "webgl", why: `auto: the shell's webview is ${env.shell.webview}` };
  if (isWebKitGtkUa(env.ua)) return { in_use: "dom", why: "auto: the user agent is WebKitGTK's" };
  return { in_use: "webgl", why: "auto: not WebKitGTK" };
}

interface Terminal {
  loadAddon(a: unknown): void;
  write(data: string): void;
}

interface WebglAddon {
  onContextLoss(cb: () => void): void;
  dispose(): void;
}

/** What each host wants, so a context lost after a switch to dom does not bring webgl back. */
const wanted = new WeakMap<HTMLElement, TerminalRenderer>();

/** Put `term` on `mode`: webgl attaches the shared-context addon (once; a lost
 *  context re-attaches while webgl is still wanted); dom disposes it (xterm's
 *  own DomRenderer comes back). Sets host.dataset.renderer to what is really
 *  drawing ("dom" when WebGL2 is unavailable), and host.webgl to the live addon
 *  or undefined (E66 clears its atlas). */
export async function setRenderer(term: Terminal, host: HTMLElement, mode: TerminalRenderer): Promise<void> {
  wanted.set(host, mode);
  const h = host as unknown as { webgl?: WebglAddon };
  if (mode === "dom") {
    const a = h.webgl;
    h.webgl = undefined;
    a?.dispose();
    host.dataset["renderer"] = "dom";
    return;
  }
  if (h.webgl) {
    host.dataset["renderer"] = "webgl";
    return;
  }
  const { WebglAddon } = (await import("@xterm/addon-webgl")) as { WebglAddon: new () => WebglAddon };
  // a switch back to dom while the addon loaded
  if (wanted.get(host) !== "webgl" || h.webgl) return;
  const attach = (): void => {
    try {
      const a = new WebglAddon();
      a.onContextLoss(() => {
        if (h.webgl === a) h.webgl = undefined;
        a.dispose();
        if (wanted.get(host) === "webgl") attach();
        else host.dataset["renderer"] = "dom";
      });
      term.loadAddon(a);
      h.webgl = a;
      host.dataset["renderer"] = "webgl";
    } catch (err) {
      host.dataset["renderer"] = "dom";
      console.error(`[terminal] the WebGL renderer is unavailable: ${(err as Error).message}`);
      term.write(`\r\n[the WebGL renderer is unavailable: ${(err as Error).message}]\r\n`);
    }
  };
  attach();
}
