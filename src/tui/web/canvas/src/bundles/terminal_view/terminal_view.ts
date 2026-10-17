import { useSharedWebgl } from "./renderer.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { Host } from "../../lib.ts";
import { hostRecordFresh } from "../canvas/host_views.ts";
import { attach as attachTo, launched, pressed, runLine, savedCmd, shortCmd, stoppedText } from "./last_cmd.ts";
import type { TermState } from "./last_cmd.ts";

// terminal_view — inline xterm.js view-agent, rendered INLINE into a canvas
// frame body (no iframe), drawing ONE PTY job of its exec_bridge backend over
// the bridge. The canvas drives the pair alone: it creates the exec agent with
// `shell: "login"` (the child's OWN `$SHELL -l`, found in the environment the
// record passes down — nobody names the program), and `boot` starts nothing, so
// attach is this view's: `status` finds a live PTY job and re-attaches (its
// `output` ring replays), else `start {pty}` — no command, no args: the record's
// own shell line (`$SHELL -l`; the kernel knows the OS's login flag, if any).
// When the job ends the frame closes — view and exec agent together, as ✕ does. xterm is the
// vendored UMD bundle (sets globalThis.Terminal / globalThis.FitAddon), lazily
// imported only when a terminal actually mounts. Preserves VSCode-style flow
// control (ack), binary image paste, no-reflow resize, and explicit autoscroll.

const ACK_SIZE = 5000; // CHAR_COUNT_ACK_SIZE — VSCode's AckDataBufferer
/** What an off-screen terminal holds before backpressure takes over (chars). */
const HOLD_MAX = 2_000_000;
// how long a re-attach waits for the program to answer the one-row shrink
// before it restores the size (an idle shell may answer nothing)
const REPAINT_WAIT_MS = 300;
// and at least this long, answered or not: a Windows console (ConPTY) redraws
// its own buffer on every resize, at once — output that is no sign the program
// has read the smaller size (one polling it every 50 ms never saw it)
const REPAINT_MIN_MS = 150;

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
  reset(): void;
  refresh(start: number, end: number): void;
  scrollToBottom(): void;
  dispose(): void;
}

interface Job {
  job_id?: number;
  pty?: boolean;
  status?: string;
}

export const terminalView: ViewBundle = {
  // The frontend member IS a `terminal_view.ts` agent (carrying a `backend_id`
  // peer ref to its exec_bridge agent); this bundle renders it.
  handles: ["terminal_view.ts"],
  fronts: "exec_bridge.tools",
  grants: (backend) => ({
    [backend]: ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"],
  }),
  // the exec agent: the project dir, the human's whole environment (its SHELL
  // included), and exactly one line it may start — the child's `$SHELL -l`
  backendFields: {
    cwd: ".",
    inherit_env: ["*"],
    shell: "login",
    ingress_rule: "allow_all_ingress",
  },

  async mount(ctx: ViewContext): Promise<ViewHandle> {
    const { kernel, mount, selfId, backend, close, record } = ctx;
    // `fantastic canvas run COMMAND…`: the cell names the program (its exec agent
    // may run exactly that); else the child's login shell
    const argv = Array.isArray(record?.["command"]) ? (record!["command"] as unknown[]).filter((a): a is string => typeof a === "string") : [];
    const host = new Host(kernel, selfId);

    // the vendored UMD bundles: an ES module (the canvas package is
    // `"type": "module"`), so the UMD takes its GLOBAL branch — importing them
    // sets globalThis.Terminal / globalThis.FitAddon.FitAddon, the one export
    await import("@xterm/xterm");
    await import("@xterm/addon-fit");
    const g = globalThis as unknown as Partial<XTermGlobals>;
    const TerminalCtor = g.Terminal;
    const FitAddonCtor = g.FitAddon?.FitAddon;
    if (typeof TerminalCtor !== "function") throw new Error("terminal_view: the vendored xterm set no globalThis.Terminal");
    if (typeof FitAddonCtor !== "function") throw new Error("terminal_view: the vendored addon-fit set no globalThis.FitAddon.FitAddon");
    const term = new TerminalCtor({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: "'SF Mono', 'Menlo', monospace",
      allowTransparency: true,
      reflowOnResize: false, // freeze old lines; new output wraps at current width
      theme: { background: "#00000000", foreground: "#e5e5e5", cursor: "#e5e5e5" },
    });
    const fit = new FitAddonCtor() as { fit(): void };
    term.loadAddon(fit);
    term.open(mount);
    // The padding goes on the TERMINAL element, not on `mount`: the fit addon
    // measures the parent's computed box (with border-box that box INCLUDES the
    // parent's padding) and subtracts only the `.xterm` element's own padding, so
    // a padded parent sized the grid past the visible area and clipped the last
    // column and row (xterm.js #1283). Here it floors over what is really free.
    const xtEl = (term as unknown as { element?: HTMLElement }).element;
    if (xtEl) {
      xtEl.style.padding = "6px 14px 6px 8px";
      xtEl.style.boxSizing = "border-box";
      xtEl.style.height = "100%";
    }
    // the renderer: its own piece (renderer.ts) — the shared WebGL one
    await useSharedWebgl(term, mount);
    // the frame's terminal, for a probe that reads what it shows (E65/E66): a
    // canvas renderer has no text in the DOM
    (mount as unknown as { xterm?: XTerm }).xterm = term;

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
    // EVENT-DRIVEN: output (or the toggle) asks for ONE scroll on the next
    // animation frame — no timer runs while the terminal is idle
    let scrollQueued = false;
    const scrollSoon = (): void => {
      if (!autoscroll || scrollQueued) return;
      scrollQueued = true;
      requestAnimationFrame(() => {
        scrollQueued = false;
        if (autoscroll) term.scrollToBottom();
      });
    };

    // flow control: ack AFTER xterm has parsed a chunk (write callback),
    // buffered to one ack per ACK_SIZE chars — real backpressure on the PTY.
    // THE JOB this frame draws; frames of any other job are not its own.
    // Frames arriving before the job is known (between `start` and its reply)
    // are held, then drawn once it is.
    // the job's kernel task id — an INTEGER, compared as one
    let jobId: number | undefined;
    const early: Array<Record<string, unknown>> = [];

    let unsentAck = 0;
    const ackChars = (n: number): void => {
      if (jobId === undefined) return;
      unsentAck += n;
      while (unsentAck >= ACK_SIZE) {
        unsentAck -= ACK_SIZE;
        void host.call(backend, { type: "ack", job_id: jobId, chars: ACK_SIZE });
      }
    };
    // OFF SCREEN, OUTPUT IS HELD (owner 2026-10-09, the load test; xterm.js
    // #6208: a paused terminal still re-renders every row on each scroll): a
    // terminal no one can see parses nothing — its output waits here and is
    // written in ONE go the moment it scrolls into view. On screen nothing
    // changes: each chunk is written as it lands. Held chunks are acked as they
    // land (the shell keeps running) up to HOLD_MAX chars; past that the acks
    // stop and the PTY's own backpressure holds the shell.
    let onScreen = true;
    let held: string[] = [];
    let heldChars = 0;
    const writeNow = (d: string): void => {
      term.write(d, () => {
        ackChars(d.length);
        scrollSoon();
      });
    };
    const draw = (e: Record<string, unknown>): void => {
      if (e["job_id"] !== jobId) return;
      const d = typeof e["data"] === "string" ? (e["data"] as string) : "";
      if (onScreen) return writeNow(d);
      held.push(d);
      heldChars += d.length;
      if (heldChars <= HOLD_MAX) ackChars(d.length);
    };
    const release = (): void => {
      if (held.length === 0) return;
      const all = held.join("");
      const acked = Math.min(heldChars, HOLD_MAX);
      held = [];
      heldChars = 0;
      term.write(all, () => {
        ackChars(all.length - acked); // what was not acked while held
        scrollSoon();
      });
    };
    const io = new IntersectionObserver((es) => {
      const vis = es.some((x) => x.isIntersecting);
      if (vis === onScreen) return;
      onScreen = vis;
      if (vis) release();
    });
    io.observe(mount);
    let done = false;
    const ended = (e: Record<string, unknown>): void => {
      if (e["job_id"] !== jobId || done) return;
      done = true;
      close();
    };

    // held while an attach runs too: its ring replay covers what came before it
    let attaching = false;
    // resolved by the job's next output frame while an attach holds them
    let answered: (() => void) | undefined;
    const offOutput = host.on(backend, "output", (e) => {
      // the PTY's bytes only: a call's own `output` event (the ring replay this
      // frame asked for) carries no `data` — it is not the program answering
      if (typeof e["data"] !== "string") return;
      if (jobId !== undefined && !attaching) return draw(e);
      early.push(e);
      if (e["job_id"] === jobId) answered?.();
    });
    const offDone = host.on(backend, "job_done", (e) => (jobId === undefined || attaching ? early.push(e) : ended(e)));

    // THE LAST LAUNCHED COMMAND (owner 2026-10-12, `last_cmd.ts`): a bar on the
    // cell. NOTHING STARTS ON ITS OWN: past its first start (the placement), a
    // cell with no live job shows it STOPPED — ▶ start, and for a shell ▶ run its
    // last command — and only the human's press starts it (TOS)
    let last: TermState = { cmd: savedCmd(await hostRecordFresh(backend).catch(() => undefined)), stopped: false };
    let started = record?.["started"] === true;
    const bar = document.createElement("div");
    bar.className = "term-last";
    bar.innerHTML = `<span class="cmd"></span>`;
    mount.appendChild(bar);
    const words = stoppedText(argv);
    const over = document.createElement("div");
    over.className = "term-stopped";
    over.innerHTML = `<div class="box"><div class="title"></div><div class="note"></div><div class="row"><button class="start">▶ start</button><button class="again"></button></div></div>`;
    (over.querySelector(".title") as HTMLElement).textContent = words.title;
    (over.querySelector(".note") as HTMLElement).textContent = words.note;
    mount.appendChild(over);
    const drawLast = (): void => {
      bar.hidden = last.cmd === null;
      const c = bar.querySelector(".cmd") as HTMLElement;
      c.textContent = last.cmd === null ? "" : shortCmd(last.cmd);
      c.title = last.cmd ?? "";
      over.hidden = !last.stopped;
      const again = over.querySelector(".again") as HTMLButtonElement;
      // a shell's last command, run again in the new shell
      again.hidden = argv.length > 0 || last.cmd === null;
      again.textContent = last.cmd === null ? "" : `▶ run ${shortCmd(last.cmd, 40)}`;
      again.title = last.cmd ?? "";
    };
    const offLast = host.on(backend, "last_cmd", (e) => {
      if (typeof e["cmd"] !== "string") return;
      last = launched(last, e["cmd"]);
      drawLast();
    });
    drawLast();

    // THE INPUT BOX holds nothing typed (owner 2026-10-16: a typed line came out
    // repeated from its start). xterm empties its hidden textarea only on Enter,
    // Ctrl-C or blur, so the whole line sits in it; a text service of the OS
    // working on that box (Safari's inline suggestions, Writing Tools) can hand
    // the whole line back as new input. The services are off, and the box is
    // emptied after each input — not while an input method composes.
    const box = (term as unknown as { textarea?: HTMLTextAreaElement }).textarea;
    let composing = false;
    if (box) {
      box.setAttribute("writingsuggestions", "false");
      box.setAttribute("autocomplete", "off");
      box.addEventListener("compositionstart", () => (composing = true));
      box.addEventListener("compositionend", () => (composing = false));
    }
    term.onData((data: string) => {
      if (jobId !== undefined) void host.call(backend, { type: "write", job_id: jobId, data });
      if (box) setTimeout(() => !composing && (box.value = ""), 0);
    });
    term.onResize(({ cols, rows }) => {
      if (jobId !== undefined) void host.call(backend, { type: "resize", job_id: jobId, cols, rows });
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
            if (jobId === undefined) return;
            await host.callBinary(backend, {
              type: "paste_image",
              job_id: jobId,
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


    const tightFit = (): void => {
      try {
        fit.fit();
        term.refresh(0, term.rows - 1);
        // Autoscroll must keep working AFTER a resize: fit() may have shifted
        // the bottom row (reflowOnResize is off, so the buffer doesn't re-pin
        // itself), so re-anchor to the new bottom here. Resize-first, so resize
        // is never gated by autoscroll.
        if (autoscroll) term.scrollToBottom();
      } catch {
        /* layout still settling */
      }
    };
    let roTimer: ReturnType<typeof setTimeout> | undefined;
    // Coalescing debounce — NOT a reset-on-every-event debounce. The autoscroll
    // tick calls term.scrollToBottom() every 100ms, and the resulting renderer
    // churn nudges the observed mount, so a reset-style debounce (clearTimeout
    // on every RO callback) would be perpetually starved by the autoscroll tick
    // and tightFit would never fire — autoscroll-on silently killing resize.
    // Instead, the FIRST RO callback after a quiet period arms a fixed-deadline
    // timer that always elapses; later callbacks during that window are absorbed
    // (80ms to let the RO burst settle, then a rAF so layout has committed).
    const ro = new ResizeObserver(() => {
      if (roTimer) return; // a fit is already scheduled — coalesce
      roTimer = setTimeout(() => {
        roTimer = undefined;
        requestAnimationFrame(tightFit);
      }, 80);
    });
    ro.observe(mount);

    // init order: fit (cols/rows) → a live job is resized to them, then its
    // ring replays (vim/tmux must replay against the right dimensions); none
    // → start the child's login shell AT them.
    tightFit();
    // ATTACH: the backend's live PTY job, resized to the grid and its ring
    // replayed; none → its login shell started at the grid. Run at mount AND
    // whenever the host comes back (owner 2026-10-09, `sudo mactop` frozen after
    // a restart): a restarted host has no job — the one this frame held died with
    // it, so writes went nowhere and the screen stayed on its last frame.
    const attach = async (again: boolean): Promise<void> => {
      if (again) {
        jobId = undefined;
        held = [];
        heldChars = 0;
        term.reset();
      }
      attaching = true;
      // how many held frames the ring replay already holds (they landed before its reply)
      let covered = 0;
      const st = (await host.call(backend, { type: "status" })) as { jobs?: Job[]; error?: string };
      const live = (st?.jobs ?? []).find((j) => j.pty === true && j.status === "running");
      const next = attachTo(last, typeof live?.job_id === "number", started);
      last = next.state;
      if (typeof live?.job_id === "number") {
        jobId = live.job_id;
        await host.call(backend, { type: "resize", job_id: jobId, cols: term.cols, rows: term.rows });
        const replay = (await host.call(backend, { type: "output", job_id: jobId })) as { output?: string };
        if (replay && typeof replay.output === "string") {
          covered = early.length;
          term.write(replay.output);
        }
        // THE PROGRAM REDRAWS (owner 2026-10-13, half-blank cells after a reload):
        // the ring drops its oldest bytes, so a full-screen program's last full
        // paint is often gone and the replay holds only partial updates; a resize
        // to the size it already has sends no SIGWINCH. One row off and back does,
        // and the program repaints the whole screen — but only once it has SEEN
        // the smaller size: back-to-back, htop reads the size after both, finds it
        // unchanged and repaints nothing (measured: 0 bytes; 50 ms apart, a full
        // screen; `sudo mactop` stayed blank the same way). So the size comes back after the
        // program's next output after the shrink took effect, or REPAINT_WAIT_MS.
        await host.call(backend, { type: "resize", job_id: jobId, cols: term.cols, rows: Math.max(1, term.rows - 1) });
        await Promise.all([
          new Promise<void>((res) => {
            answered = res;
            setTimeout(res, REPAINT_WAIT_MS);
          }),
          new Promise<void>((res) => setTimeout(res, REPAINT_MIN_MS)),
        ]);
        answered = undefined;
        await host.call(backend, { type: "resize", job_id: jobId, cols: term.cols, rows: term.rows });
      } else if (next.start) {
        await startJob();
      }
      attaching = false;
      drainEarly(covered);
      drawLast();
    };
    // the frames held while the job was unknown (or an attach ran), in order;
    // the first `covered` are in a ring replay already
    const drainEarly = (covered: number): void => {
      early.splice(0).forEach((e, i) => {
        if (e["type"] === "job_done") return ended(e);
        // in the replay already: not drawn again, acked once (it was sent once)
        if (i < covered && e["job_id"] === jobId) return ackChars(typeof e["data"] === "string" ? (e["data"] as string).length : 0);
        draw(e);
      });
    };
    // START the cell's program (its first placement, or the human's ▶): once
    // started, the cell records it — later it waits for ▶ instead
    const startJob = async (): Promise<boolean> => {
      const r = (await host.call(backend, {
        type: "start",
        ...(argv.length ? { command: argv[0]!, args: argv.slice(1) } : {}),
        pty: { cols: term.cols, rows: term.rows },
      })) as { job_id?: number; error?: string; reason?: string };
      if (typeof r?.job_id !== "number") {
        term.write(`\r\n[${argv.length ? argv.join(" ") : "the login shell"} did not start: ${r?.error ?? JSON.stringify(r)}]\r\n`);
        return false;
      }
      jobId = r.job_id;
      if (!started) {
        started = true;
        kernel.updateMeta(selfId, { started: true } as never);
      }
      return true;
    };
    const press = async (run: boolean): Promise<void> => {
      if (jobId !== undefined) return;
      term.reset();
      const ok = await startJob();
      // what the program wrote before the start's reply named its job (a
      // shell's first prompt, on a slow host) was held: it is drawn now
      drainEarly(0);
      if (!ok) return;
      if (run && last.cmd !== null && jobId !== undefined) void host.call(backend, { type: "write", job_id: jobId, data: runLine(last.cmd) });
      last = pressed(last);
      drawLast();
    };
    (over.querySelector(".start") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      void press(false);
    });
    (over.querySelector(".again") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      void press(true);
    });
    await attach(false);
    let lost = false;
    const offLifecycle = host.onLifecycle(backend, (state) => {
      if (state === "disconnected") lost = true;
      else if (lost) {
        lost = false;
        void attach(true);
      }
    });

    return {
      headerButtons: [
        {
          id: "autoscroll",
          glyph: "↧",
          title: "autoscroll to bottom on output",
          toggle: true,
          onClick: (): boolean => {
            autoscroll = !autoscroll;
            scrollSoon();
            return autoscroll;
          },
        },
      ],
      unmount(): void {
        io.disconnect();
        offLifecycle();
        offOutput();
        offDone();
        offLast();
        bar.remove();
        over.remove();
        host.unwatch(backend);
        clearTimeout(roTimer);
        ro.disconnect();
        mount.removeEventListener("paste", onPaste);
        term.dispose();
      },
    };
  },
};
