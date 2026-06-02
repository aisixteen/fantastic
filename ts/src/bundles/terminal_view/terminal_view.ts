import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { Host } from "../host.ts";

// terminal_view — inline xterm.js view-agent. Ported from
// terminal_webapp/index.html, but rendered INLINE into a canvas frame body
// (no iframe), talking to its terminal_backend over the bridge. xterm is the
// vendored UMD bundle (sets globalThis.Terminal / globalThis.FitAddon), lazily
// imported only when a terminal actually mounts. Preserves VSCode-style flow
// control (ack), binary image paste, no-reflow resize, and explicit autoscroll.

const ACK_SIZE = 5000; // CHAR_COUNT_ACK_SIZE — VSCode's AckDataBufferer

interface XTermGlobals {
  Terminal: new (opts: unknown) => XTerm;
  FitAddon: { FitAddon: new () => unknown };
}
interface XTerm {
  cols: number;
  rows: number;
  loadAddon(a: unknown): void;
  open(el: HTMLElement): void;
  onData(cb: (data: string) => void): void;
  onResize(cb: (size: { cols: number; rows: number }) => void): void;
  write(data: string, cb?: () => void): void;
  refresh(start: number, end: number): void;
  scrollToBottom(): void;
  dispose(): void;
}

export const terminalView: ViewBundle = {
  // The frontend member IS a `terminal_view.ts` agent (carrying a `backend_id`
  // peer ref to a host terminal_backend); this bundle renders it.
  handles: ["terminal_view.ts"],

  async mount(ctx: ViewContext): Promise<ViewHandle> {
    const { kernel, mount, selfId, backend } = ctx;
    const host = new Host(kernel, selfId);

    // lazy-load the vendored UMD bundles (resolved via the page import map);
    // they attach Terminal / FitAddon to globalThis as a side effect.
    await import("@xterm/xterm");
    await import("@xterm/addon-fit");
    const g = globalThis as unknown as XTermGlobals;
    const term = new g.Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: "'SF Mono', 'Menlo', monospace",
      allowTransparency: true,
      reflowOnResize: false, // freeze old lines; new output wraps at current width
      theme: { background: "#00000000", foreground: "#e5e5e5", cursor: "#e5e5e5" },
    });
    const fit = new g.FitAddon.FitAddon() as { fit(): void };
    term.loadAddon(fit);
    mount.style.padding = "6px 14px 6px 8px";
    mount.style.boxSizing = "border-box";
    term.open(mount);

    // wait for webfonts before measuring cell metrics (xterm #1164/#1534/#2630)
    const fonts = (document as unknown as { fonts?: { ready: Promise<unknown> } }).fonts;
    if (fonts?.ready) {
      try {
        await fonts.ready;
      } catch {
        /* fonts API absent / rejected — measure with what's resolved */
      }
    }

    let autoscroll = false;
    let pendingOutput = false;

    // flow control: ack AFTER xterm has parsed a chunk (write callback),
    // buffered to one ack per ACK_SIZE chars — real backpressure on the PTY.
    let unsentAck = 0;
    const ackChars = (n: number): void => {
      unsentAck += n;
      while (unsentAck >= ACK_SIZE) {
        unsentAck -= ACK_SIZE;
        void host.call(backend, { type: "ack", chars: ACK_SIZE });
      }
    };

    const offOutput = host.on(backend, "output", (e) => {
      const d = typeof e["data"] === "string" ? (e["data"] as string) : "";
      term.write(d, () => ackChars(d.length));
      if (autoscroll) pendingOutput = true;
    });
    const offClosed = host.on(backend, "closed", () => {
      term.write("\r\n[process closed]\r\n");
    });

    term.onData((data: string) => {
      void host.call(backend, { type: "write", data });
    });
    term.onResize(({ cols, rows }) => {
      void host.call(backend, { type: "resize", cols, rows });
    });

    // image paste → binary frame to the backend (it saves a file + types the
    // path into the PTY). Text paste is left to xterm.
    const onPaste = async (ev: ClipboardEvent): Promise<void> => {
      const items = ev.clipboardData?.items ?? [];
      for (const it of items) {
        if (it.kind === "file" && it.type.startsWith("image/")) {
          ev.preventDefault();
          const file = it.getAsFile();
          if (!file) continue;
          try {
            const buf = await file.arrayBuffer();
            await host.callBinary(backend, {
              type: "paste_image",
              data: new Uint8Array(buf),
              mime: it.type,
            });
          } catch (err) {
            term.write(`\r\n[image paste failed: ${(err as Error).message}]\r\n`);
          }
          return;
        }
      }
    };
    mount.addEventListener("paste", onPaste);

    const scrollTimer = setInterval(() => {
      if (autoscroll && pendingOutput) {
        term.scrollToBottom();
        pendingOutput = false;
      }
    }, 100);

    const tightFit = (): void => {
      try {
        fit.fit();
        term.refresh(0, term.rows - 1);
      } catch {
        /* layout still settling */
      }
    };
    let roTimer: ReturnType<typeof setTimeout> | undefined;
    const ro = new ResizeObserver(() => {
      clearTimeout(roTimer);
      roTimer = setTimeout(() => requestAnimationFrame(tightFit), 80);
    });
    ro.observe(mount);

    // init order: fit (cols/rows) → boot PTY → resize PTY → replay scrollback.
    // Otherwise vim/tmux replay against the wrong dimensions and corrupt redraw.
    tightFit();
    await host.call(backend, { type: "boot" });
    await host.call(backend, { type: "resize", cols: term.cols, rows: term.rows });
    const replay = (await host.call(backend, { type: "output" })) as { output?: string };
    if (replay && typeof replay.output === "string") term.write(replay.output);

    return {
      headerButtons: [
        {
          id: "autoscroll",
          glyph: "↧",
          title: "autoscroll to bottom on output",
          toggle: true,
          onClick: (): boolean => {
            autoscroll = !autoscroll;
            if (autoscroll) pendingOutput = true;
            return autoscroll;
          },
        },
      ],
      unmount(): void {
        offOutput();
        offClosed();
        host.unwatch(backend);
        clearInterval(scrollTimer);
        clearTimeout(roTimer);
        ro.disconnect();
        mount.removeEventListener("paste", onPaste);
        term.dispose();
      },
    };
  },
};
