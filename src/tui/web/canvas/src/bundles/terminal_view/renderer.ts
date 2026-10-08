// THE TERMINAL RENDERER (owner 2026-10-09) — its own piece, the DEFAULT for
// every terminal cell (no switcher yet): xterm's WebGL renderer through the
// page's ONE shared WebGL context — the patched `@xterm/addon-webgl`
// (`vendor-src/addon-webgl`, provenance in `vendor/VENDOR.md`). Measured (E65
// load, headful GPU): xterm's DOM renderer re-rasterized every line's text and
// fell to ~5 fps at 40 streaming terminals on screen; this holds 60 fps at 200.
// It knows nothing of the canvas: it takes a terminal and the element it lives
// in.
//
// A lost context: the addon is dropped and a fresh one made (it acquires a new
// shared context). No WebGL2: said loud, in the terminal and the console —
// xterm keeps drawing with the DOM renderer it starts with.

interface Terminal {
  loadAddon(a: unknown): void;
  write(data: string): void;
}

interface WebglAddon {
  onContextLoss(cb: () => void): void;
  dispose(): void;
}

/** Put `term` on the shared WebGL renderer. `host` (the element the terminal
 *  lives in) carries the live addon as `host.webgl`, for a probe (E66 clears
 *  its atlas). */
export async function useSharedWebgl(term: Terminal, host: HTMLElement): Promise<void> {
  const { WebglAddon } = (await import("@xterm/addon-webgl")) as { WebglAddon: new () => WebglAddon };
  const attach = (): void => {
    try {
      const a = new WebglAddon();
      a.onContextLoss(() => {
        a.dispose();
        attach();
      });
      term.loadAddon(a);
      (host as unknown as { webgl?: WebglAddon }).webgl = a;
    } catch (err) {
      console.error(`[terminal] the WebGL renderer is unavailable: ${(err as Error).message}`);
      term.write(`\r\n[the WebGL renderer is unavailable: ${(err as Error).message}]\r\n`);
    }
  };
  attach();
}
