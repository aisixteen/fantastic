// Zero-dependency headless-Chrome driver over the Chrome DevTools Protocol.
// Launches the SYSTEM Chrome (no npm dep, no browser download) and drives it via
// CDP over Node's built-in WebSocket. Used by *.browser.itest.ts; the test skips
// cleanly when `chromeAvailable()` is false (Chrome not installed).
//
// The dance: spawn `chrome --headless --remote-debugging-port=0`, read the chosen
// port from `<user-data-dir>/DevToolsActivePort`, open the browser-level WS, then
// Target.createTarget → attachToTarget(flatten) to get a page session. Session
// commands ride the same socket tagged with `sessionId`. `evaluate` runs JS in
// the page and returns its value (awaiting promises) — enough to assert the DOM.

import { spawn } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync, existsSync, readFileSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

/** True if the system Chrome is present (else the browser itest skips). */
export function chromeAvailable(): boolean {
  try {
    return statSync(CHROME).isFile();
  } catch {
    return false;
  }
}

const sleep = (ms: number): Promise<void> =>
  new Promise((r) => setTimeout(r, ms));

interface Pending {
  resolve: (v: Record<string, unknown>) => void;
  reject: (e: Error) => void;
}

/** A headless Chrome + one page session, driven over CDP. */
export class Browser {
  private msgId = 0;
  private readonly pending = new Map<number, Pending>();
  private sessionId = "";
  /** page-side errors (uncaught exceptions + console.error) for diagnostics. */
  readonly pageErrors: string[] = [];
  private readonly proc: ChildProcess;
  private readonly userDir: string;
  private readonly ws: WebSocket;
  private mainFrameId = "";
  /** CDP execution-context id -> its frameId (for evaluating inside iframes). */
  private readonly contexts = new Map<number, string>();

  private constructor(proc: ChildProcess, userDir: string, ws: WebSocket) {
    this.proc = proc;
    this.userDir = userDir;
    this.ws = ws;
    ws.addEventListener("message", (ev: MessageEvent) => this.onMessage(ev));
  }

  /** Launch Chrome, attach to a fresh page target, return the driver. */
  static async launch(): Promise<Browser> {
    const userDir = mkdtempSync(join(tmpdir(), "ftchrome-"));
    const proc = spawn(
      CHROME,
      [
        "--headless=new",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-dev-shm-usage",
        // keep sandboxed (null-origin) iframes in-process so CDP can evaluate in
        // them via the page session (they'd otherwise be OOPIFs). Test-driver
        // only — the production iframe sandbox is unchanged.
        "--disable-site-isolation-trials",
        "--disable-features=IsolateOrigins,site-per-process",
        "--remote-debugging-port=0",
        `--user-data-dir=${userDir}`,
        "about:blank",
      ],
      { stdio: "ignore" },
    );

    // Chrome writes the chosen devtools port + ws path here once it's up.
    const portFile = join(userDir, "DevToolsActivePort");
    let browserWsUrl = "";
    const start = Date.now();
    while (Date.now() - start < 15000) {
      if (existsSync(portFile)) {
        const lines = readFileSync(portFile, "utf8").trim().split("\n");
        if (lines.length >= 2 && lines[0] !== "") {
          browserWsUrl = `ws://127.0.0.1:${lines[0]}${lines[1]}`;
          break;
        }
      }
      await sleep(100);
    }
    if (browserWsUrl === "") {
      proc.kill("SIGKILL");
      rmSync(userDir, { recursive: true, force: true });
      throw new Error("chrome devtools endpoint never appeared");
    }

    const ws = new WebSocket(browserWsUrl);
    await new Promise<void>((resolve, reject) => {
      ws.addEventListener("open", () => resolve(), { once: true });
      ws.addEventListener("error", () => reject(new Error("browser ws error")), {
        once: true,
      });
    });

    const browser = new Browser(proc, userDir, ws);
    const { targetId } = (await browser.rawSend("Target.createTarget", {
      url: "about:blank",
    })) as { targetId: string };
    const { sessionId } = (await browser.rawSend("Target.attachToTarget", {
      targetId,
      flatten: true,
    })) as { sessionId: string };
    browser.sessionId = sessionId;
    await browser.send("Page.enable", {});
    await browser.send("Runtime.enable", {});
    const ft = (await browser.send("Page.getFrameTree", {})) as {
      frameTree?: { frame?: { id?: string } };
    };
    browser.mainFrameId = ft.frameTree?.frame?.id ?? "";
    return browser;
  }

  private onMessage(ev: MessageEvent): void {
    const msg = JSON.parse(String(ev.data)) as {
      id?: number;
      result?: Record<string, unknown>;
      error?: { message: string };
      method?: string;
      params?: Record<string, unknown>;
    };
    if (typeof msg.id === "number") {
      const p = this.pending.get(msg.id);
      if (p === undefined) return;
      this.pending.delete(msg.id);
      if (msg.error) p.reject(new Error(msg.error.message));
      else p.resolve(msg.result ?? {});
      return;
    }
    // track execution contexts so we can evaluate INSIDE the srcdoc iframe
    // (a null-origin sandbox the parent DOM can't reach, but CDP can).
    if (msg.method === "Runtime.executionContextCreated") {
      const ctx = (msg.params?.["context"] ?? {}) as {
        id?: number;
        auxData?: { frameId?: string };
      };
      if (typeof ctx.id === "number") {
        this.contexts.set(ctx.id, ctx.auxData?.frameId ?? "");
      }
      return;
    }
    if (msg.method === "Runtime.executionContextDestroyed") {
      const cid = msg.params?.["executionContextId"];
      if (typeof cid === "number") this.contexts.delete(cid);
      return;
    }
    if (msg.method === "Runtime.executionContextsCleared") {
      this.contexts.clear();
      return;
    }
    // events — collect page-side failures for diagnostics
    if (msg.method === "Runtime.exceptionThrown") {
      const d = (msg.params?.["exceptionDetails"] ?? {}) as {
        exception?: { description?: string };
        text?: string;
      };
      this.pageErrors.push(d.exception?.description ?? d.text ?? "exception");
    } else if (msg.method === "Runtime.consoleAPICalled") {
      const p = msg.params as { type?: string; args?: { value?: unknown }[] };
      if (p.type === "error") {
        this.pageErrors.push(p.args?.map((a) => String(a.value)).join(" ") ?? "");
      }
    }
  }

  private rawSend(
    method: string,
    params: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    const id = ++this.msgId;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  private send(
    method: string,
    params: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    const id = ++this.msgId;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params, sessionId: this.sessionId }));
    });
  }

  /** Navigate the page and wait for the load event. */
  async goto(url: string): Promise<void> {
    await this.send("Page.navigate", { url });
    // give the document a moment; callers then poll `evaluate` for readiness
    await sleep(200);
  }

  /** Run an expression in the page; awaits promises, returns the value. */
  async evaluate<T>(expression: string): Promise<T> {
    const r = (await this.send("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
    })) as {
      result?: { value?: T };
      exceptionDetails?: { exception?: { description?: string }; text?: string };
    };
    if (r.exceptionDetails) {
      const d = r.exceptionDetails;
      throw new Error(`page evaluate threw: ${d.exception?.description ?? d.text}`);
    }
    return r.result?.value as T;
  }

  /** Execution-context ids belonging to SUB-frames (the srcdoc iframes). CDP can
   *  evaluate there even though they are null-origin sandboxes the parent DOM
   *  can't touch. */
  private subframeContextIds(): number[] {
    const out: number[] = [];
    for (const [cid, frameId] of this.contexts) {
      if (frameId !== "" && frameId !== this.mainFrameId) out.push(cid);
    }
    return out;
  }

  private async evalInContext<T>(cid: number, expression: string): Promise<T> {
    const r = (await this.send("Runtime.evaluate", {
      expression,
      contextId: cid,
      awaitPromise: true,
      returnByValue: true,
    })) as {
      result?: { value?: T };
      exceptionDetails?: { exception?: { description?: string }; text?: string };
    };
    if (r.exceptionDetails) {
      const d = r.exceptionDetails;
      throw new Error(`iframe evaluate threw: ${d.exception?.description ?? d.text}`);
    }
    return r.result?.value as T;
  }

  /** Evaluate inside EACH srcdoc iframe; return the first non-null/undefined
   *  result (waits up to `ms` for a sub-frame to appear). Use an expression /
   *  selector unique to the target panel to disambiguate across iframes. */
  async evalInAnyIframe<T>(expression: string, ms = 10000): Promise<T | null> {
    const start = Date.now();
    for (;;) {
      for (const cid of this.subframeContextIds()) {
        try {
          const v = await this.evalInContext<T>(cid, expression);
          if (v !== null && v !== undefined) return v;
        } catch {
          /* a frame may be mid-navigation/torn down — try the next */
        }
      }
      if (Date.now() - start >= ms) return null;
      await sleep(100);
    }
  }

  /** Evaluate inside EVERY srcdoc iframe; collect the non-null results (waits
   *  up to `ms` for at least one sub-frame). Use to compare across panels. */
  async evalAllIframes<T>(expression: string, ms = 10000): Promise<T[]> {
    const start = Date.now();
    while (this.subframeContextIds().length === 0 && Date.now() - start < ms) {
      await sleep(100);
    }
    const out: T[] = [];
    for (const cid of this.subframeContextIds()) {
      try {
        const v = await this.evalInContext<T>(cid, expression);
        if (v !== null && v !== undefined) out.push(v);
      } catch {
        /* skip a torn-down frame */
      }
    }
    return out;
  }

  /** Click an element (by a selector unique to its panel) inside whichever
   *  srcdoc iframe contains it. Returns whether it was found + clicked. */
  async clickInAnyIframe(selector: string, ms = 10000): Promise<boolean> {
    const ok = await this.evalInAnyIframe<boolean>(
      `(() => { const el = document.querySelector(${JSON.stringify(selector)});
                if (!el) return null; el.click(); return true; })()`,
      ms,
    );
    return ok === true;
  }

  /** Poll `evaluate(expression)` until truthy (returns it) or timeout (throws). */
  async waitFor<T>(expression: string, ms: number): Promise<T> {
    const start = Date.now();
    let last: T = undefined as unknown as T;
    while (Date.now() - start < ms) {
      last = await this.evaluate<T>(expression);
      if (last) return last;
      await sleep(100);
    }
    throw new Error(
      `waitFor timed out after ${ms}ms: ${expression}\npage errors: ${this.pageErrors.join(" | ")}`,
    );
  }

  close(): void {
    try {
      this.ws.close();
    } catch {
      /* ignore */
    }
    try {
      this.proc.kill("SIGKILL");
    } catch {
      /* already gone */
    }
    try {
      rmSync(this.userDir, { recursive: true, force: true });
    } catch {
      /* best effort */
    }
  }
}
