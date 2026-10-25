import type { HomeRow } from "./home_model.ts";
import { activityDue, decodeLock, NO_OVERLAY, overlayFor, overlayInput, systemRow, userSocketUrl } from "./lock_model.ts";
import type { LockState, Overlay } from "./lock_model.ts";

// THE LOCK SCREEN — the DOM and the socket of `lock_model.ts`. The Home page
// dials the SYSTEM canvas (the running row named `system`) on its `/user/ws`:
// it watches the `lock` agent there, calls `unlock` and sends `activity`. The
// system canvas lets this page's origin in (`kern/home_origin`). The overlay is
// the page shell's, not a cell: nothing a key or a click does removes it, only
// a state that says `locked: false`. A UI lock, not a security boundary.
//
// It keeps the last state across a lost socket and across the row going away,
// so the screen stays locked until `unlock` says otherwise.

const RECONNECT_MS = 2000;

/** The lock channel to one system canvas: its state, `unlock`, `activity`. */
export class LockLink {
  private ws: WebSocket | null = null;
  private url: string | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private readonly pending = new Map<string, (data: unknown) => void>();
  private lastActivity: number | null = null;

  private readonly onState: (s: LockState) => void;

  constructor(onState: (s: LockState) => void) {
    this.onState = onState;
  }

  /** Follow the canvas at `canvasUrl` (null: none now). A new url reconnects. */
  follow(canvasUrl: string | null): void {
    const url = canvasUrl === null ? null : userSocketUrl(canvasUrl);
    if (url === this.url) return;
    this.url = url;
    this.drop();
    if (url !== null) this.connect(url);
  }

  private drop(): void {
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    const ws = this.ws;
    this.ws = null;
    if (ws !== null) {
      ws.onclose = null;
      ws.close();
    }
    for (const done of this.pending.values()) done(null);
    this.pending.clear();
  }

  private connect(url: string): void {
    const ws = new WebSocket(url);
    this.ws = ws;
    ws.onopen = () => {
      ws.send(JSON.stringify({ type: "watch", src: "lock" }));
      void this.call({ type: "state" }).then((d) => this.state(d));
    };
    ws.onmessage = (ev) => {
      let f: Record<string, unknown>;
      try {
        f = JSON.parse(String(ev.data)) as Record<string, unknown>;
      } catch {
        return;
      }
      if (f["type"] === "event" && f["src"] === "lock") {
        const p = f["payload"] as Record<string, unknown> | undefined;
        if (p?.["type"] === "state") this.state(p["state"]);
      } else if ((f["type"] === "reply" || f["type"] === "error") && typeof f["id"] === "string") {
        const done = this.pending.get(f["id"]);
        this.pending.delete(f["id"]);
        done?.(f["type"] === "reply" ? f["data"] : f);
      }
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      for (const done of this.pending.values()) done(null);
      this.pending.clear();
      if (this.url === url) this.timer = setTimeout(() => this.url === url && this.connect(url), RECONNECT_MS);
    };
  }

  private state(d: unknown): void {
    const s = decodeLock(d);
    if (s !== null) this.onState(s);
  }

  /** A call to the `lock` agent: its reply, or null when the socket is not there. */
  private call(payload: Record<string, unknown>): Promise<unknown> {
    const ws = this.ws;
    if (ws === null || ws.readyState !== WebSocket.OPEN) return Promise.resolve(null);
    const id = crypto.randomUUID();
    return new Promise((resolve) => {
      this.pending.set(id, resolve);
      ws.send(JSON.stringify({ type: "call", target: "user", id, payload: { type: "forward", target: "lock", payload } }));
    });
  }

  /** Try `pin`. The refusal's text, or "" when it was not refused (the state follows by event). */
  async unlock(pin: string): Promise<string> {
    const r = (await this.call({ type: "unlock", pin })) as Record<string, unknown> | null;
    if (r === null) return "The lock is not reachable";
    if (typeof r["error"] === "string") return String(r["error"]);
    this.state(r);
    return "";
  }

  /** Input on the page: sent at most once per 10 s. */
  activity(now: number): void {
    if (!activityDue(this.lastActivity, now)) return;
    this.lastActivity = now;
    void this.call({ type: "activity" });
  }

  close(): void {
    this.url = null;
    this.drop();
  }
}

/** The overlay element and its behaviour, over the Home's root. */
export class LockScreen {
  private lock: LockState | null = null;
  private overlay: Overlay = NO_OVERLAY;
  private readonly el: HTMLElement;
  private readonly input: HTMLInputElement;
  private readonly note: HTMLElement;
  private readonly link: LockLink;
  private tick: ReturnType<typeof setInterval> | null = null;
  private readonly swallow = (e: Event): void => {
    const inside = e.target instanceof Node && this.el.contains(e.target);
    const kind = e.type === "keydown" || e.type === "keyup" || e.type === "keypress" ? "key" : "click";
    if (overlayInput(this.overlay, { type: kind, key: (e as KeyboardEvent).key, inside }).swallow) {
      e.preventDefault();
      e.stopImmediatePropagation();
    }
  };
  private readonly onInput = (): void => this.link.activity(Date.now());
  private static readonly SWALLOWED = ["keydown", "keyup", "keypress", "click", "dblclick", "contextmenu", "pointerdown", "mousedown"];

  private readonly root: HTMLElement;

  constructor(root: HTMLElement) {
    this.root = root;
    this.input = document.createElement("input");
    this.input.type = "password";
    this.input.inputMode = "numeric";
    this.input.autocomplete = "off";
    this.input.placeholder = "PIN";
    this.input.maxLength = 12;
    this.note = document.createElement("p");
    this.note.className = "lock-note";
    const go = document.createElement("button");
    go.type = "button";
    go.textContent = "Unlock";
    const title = document.createElement("h1");
    title.textContent = "Locked";
    const form = document.createElement("form");
    form.className = "lock-form";
    form.append(this.input, go);
    this.el = document.createElement("div");
    this.el.className = "lock-overlay";
    this.el.hidden = true;
    this.el.append(title, form, this.note);
    const submit = async (): Promise<void> => {
      const pin = this.input.value;
      this.input.value = "";
      if (pin === "") return;
      const refused = await this.link.unlock(pin);
      if (refused !== "") this.note.textContent = refused;
    };
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      void submit();
    });
    go.addEventListener("click", () => void submit());
    this.link = new LockLink((s) => this.apply(s));
    root.append(this.el);
    // capture, first on the window: nothing reaches the page behind while locked
    for (const t of LockScreen.SWALLOWED) window.addEventListener(t, this.swallow, true);
    for (const t of ["keydown", "pointerdown", "pointermove"]) window.addEventListener(t, this.onInput, true);
    this.tick = setInterval(() => this.draw(), 1000);
  }

  /** Is the screen locked now? (The Home's own keys wait.) */
  get locked(): boolean {
    return this.overlay.shown;
  }

  /** The Home's rows changed: follow the system canvas among them. */
  rows(rows: readonly HomeRow[]): void {
    this.link.follow(systemRow(rows)?.canvas?.url ?? null);
  }

  private apply(s: LockState): void {
    const wasShown = this.overlay.shown;
    this.lock = s;
    this.draw();
    if (this.overlay.shown && !wasShown) {
      // focus leaves any canvas frame for the PIN field
      (document.activeElement as HTMLElement | null)?.blur();
      this.input.focus();
    }
  }

  private draw(): void {
    this.overlay = overlayFor(this.lock, Date.now());
    this.el.hidden = !this.overlay.shown;
    this.input.disabled = !this.overlay.canTry;
    if (this.overlay.message !== "" || !this.overlay.shown) this.note.textContent = this.overlay.message;
    // the page behind cannot be reached, even by tab: inert, with its frames
    for (const child of Array.from(this.root.children)) {
      if (child !== this.el) (child as HTMLElement).inert = this.overlay.shown;
    }
  }

  unmount(): void {
    for (const t of LockScreen.SWALLOWED) window.removeEventListener(t, this.swallow, true);
    for (const t of ["keydown", "pointerdown", "pointermove"]) window.removeEventListener(t, this.onInput, true);
    if (this.tick !== null) clearInterval(this.tick);
    this.link.close();
    this.el.remove();
  }
}
