import type { Kernel } from "../lib.ts";

// The contract for an INLINE view-agent — DOM logic the frontend kernel mounts
// directly into a canvas frame body (vs an iframe, which is reserved for
// untrusted/external content). A view bundle declares which host
// `handler_module`(s) it renders; the canvas reads each member record's
// handler_module and picks the matching bundle.

export interface ViewContext {
  /** the page kernel SCOPED TO THIS VIEW: every send through it is the view's
   *  own, checked against the view's `can_call` before the bridge sees it */
  kernel: Kernel;
  /** the frame body element to render into */
  mount: HTMLElement;
  /** this view-agent's local id (= the host member id it mirrors) */
  selfId: string;
  /** the host backend agent id this view fronts (watch/call target) */
  backend: string;
  /** this view-agent's own record — read fields like `mode` (multi-mode views) */
  record?: Record<string, unknown>;
}

/** A frame-chrome chip a view exposes. Wired locally (no browser bus — the
 *  view and the canvas share one page now). For toggles, onClick returns the
 *  new active state so the canvas can reflect it on the chip. */
export interface HeaderButton {
  id: string;
  glyph: string;
  title: string;
  toggle: boolean;
  onClick(): boolean | void;
}

export interface ViewHandle {
  /** teardown: unwatch, dispose, remove listeners */
  unmount(): void;
  /** optional frame-chrome chips this view contributes */
  headerButtons?: readonly HeaderButton[];
}

export interface ViewBundle {
  /** the frontend handler_modules this renders inline, e.g. "terminal_view.ts" */
  readonly handles: readonly string[];
  /** The EXACT host handler_module this view SPAWNS a backend of and fronts
   *  (e.g. "terminal_backend.tools") — declared, never matched by name.
   *  Absent: the view fronts a backend someone else made; it never spawns. */
  readonly fronts?: string;
  /** This view's OWN egress grant — the host targets and verbs it calls — for
   *  a bound backend. The canvas writes it into the view's `can_call` when it
   *  binds the backend: a view SENDS AS ITSELF, so it reaches its own backend
   *  and nothing else the socket leg reaches. */
  grants(backend: string): Record<string, readonly string[]>;
  mount(ctx: ViewContext): Promise<ViewHandle> | ViewHandle;
}
