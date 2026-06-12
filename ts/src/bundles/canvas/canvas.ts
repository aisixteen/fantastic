import type { Kernel } from "../../kernel/kernel.ts";
import { Agent } from "../../kernel/agent.ts";
import type { Payload } from "../../kernel/json.ts";
import { Host } from "../host.ts";
import { CANVAS_CSS } from "./styles.ts";
import type { ViewBundle, ViewHandle } from "../view.ts";
import { terminalView } from "../terminal_view/terminal_view.ts";
import { aiView } from "../ai_view/ai_view.ts";
import { htmlView } from "../html_agent/html_view.ts";
import { createGlHost } from "./gl_host.ts";
import type { GlHost, GlView } from "./gl_host.ts";

// The canvas view-AGENT: the DOM-root compositor. Owns document.body, renders
// host member agents either INLINE (a TS view-agent for first-party content,
// keyed by the member's handler_module) or as an IFRAME (untrusted/external
// content that answers get_webapp). Translates pan/zoom/drag/dblclick into
// kernel sends. The kernel knows none of this.

// inline view registry — host handler_module → TS view bundle
const VIEW_BUNDLES: readonly ViewBundle[] = [terminalView, aiView, htmlView];
function viewFor(handlerModule: unknown): ViewBundle | undefined {
  if (typeof handlerModule !== "string") return undefined;
  return VIEW_BUNDLES.find((b) => b.handles.includes(handlerModule));
}

// The canvas names NO host bundle id or type. To spawn a host peer it calls the
// HOST ROOT via `host.callHost("kernel", …)` — the host resolves `kernel` to its
// OWN root (`kernel_state`/`core`) — and it DISCOVERS which bundle provides a
// capability from the live host catalog (`reflect bundles=all`) by matching a
// capability name, instead of hardcoding a handler_module. (An LLM does the same
// from the host + view readmes; this dblclick is just the deterministic shortcut.)
const PTY_HINT = /(^|[._/-])(terminal_backend|pty)([._]|$)/i;

export interface MountOptions {
  kernel: Kernel;
  mount: HTMLElement;
  selfId: string;
}

interface Rec {
  id: string;
  [key: string]: unknown;
}
interface Webapp {
  url: string;
  default_width?: number;
  default_height?: number;
  title?: string;
}
interface Frame {
  el: HTMLElement;
  rec: Rec;
  kind: "inline" | "iframe";
  handle?: ViewHandle; // inline only
}

const num = (r: Rec, k: string): number | undefined =>
  typeof r[k] === "number" ? (r[k] as number) : undefined;
const str = (r: Rec, k: string): string | undefined =>
  typeof r[k] === "string" ? (r[k] as string) : undefined;
const bool = (r: Rec, k: string): boolean => r[k] === true;

export async function mountCanvas(opts: MountOptions): Promise<void> {
  const { kernel, mount, selfId } = opts;
  const host = new Host(kernel, selfId);

  // ─── DOM scaffold ───────────────────────────────────────────────
  const style = document.createElement("style");
  style.textContent = CANVAS_CSS;
  document.head.appendChild(style);

  const bg = document.createElement("canvas");
  bg.id = "bg";
  mount.appendChild(bg);

  const canvas = document.createElement("div");
  canvas.id = "canvas";
  const world = document.createElement("div");
  world.className = "canvas-world";
  canvas.appendChild(world);
  const toolbar = document.createElement("div");
  toolbar.id = "toolbar";
  const status = document.createElement("span");
  status.textContent = "…";
  toolbar.appendChild(status);
  mount.appendChild(canvas);
  mount.appendChild(toolbar);

  // ─── view: zoom + pan (DOM transform on .canvas-world; GL camera locked) ──
  const view = { z: 1, ox: 0, oy: 0 };
  let targetZ = view.z;
  const ZOOM_MIN = 0.1;
  const ZOOM_MAX = 5;
  const ZOOM_LERP = 0.72;
  let glHost: GlHost | null = null;
  const applyView = (): void => {
    world.style.transform = `translate(${view.ox}px, ${view.oy}px) scale(${view.z})`;
    glHost?.applyCamera();
  };
  applyView();

  // The GL host (shared THREE renderer/scene) is created lazily on the first
  // GL member, so a canvas with no GL views never loads three.
  const ensureGlHost = async (): Promise<GlHost> => {
    if (glHost === null) {
      glHost = await createGlHost({
        bg,
        view,
        host,
        selfId,
        fetchSource: async (aid) =>
          (await host.call(aid, { type: "get_gl_view" }).catch(() => null)) as GlView | null,
      });
    }
    return glHost;
  };
  window.addEventListener("resize", () => {
    glHost?.resize();
    applyView();
  });

  const tick = (): void => {
    if (view.z !== targetZ) {
      const cx = window.innerWidth / 2;
      const cy = window.innerHeight / 2;
      let nextZ = view.z + (targetZ - view.z) * ZOOM_LERP;
      if (Math.abs(targetZ - nextZ) < 0.0005) nextZ = targetZ;
      const ratio = nextZ / view.z;
      view.ox = cx - (cx - view.ox) * ratio;
      view.oy = cy - (cy - view.oy) * ratio;
      view.z = nextZ;
      applyView();
    }
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);

  canvas.addEventListener(
    "wheel",
    (e) => {
      if ((e.target as Element).closest(".agent-frame")) return;
      e.preventDefault();
      const factor = e.deltaY > 0 ? 0.95 : 1.05;
      targetZ = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, targetZ * factor));
    },
    { passive: false },
  );

  canvas.addEventListener("mousedown", (e) => {
    if ((e.target as Element).closest(".agent-frame")) return;
    if (e.button !== 0 && e.button !== 1) return;
    e.preventDefault();
    const sx = e.clientX;
    const sy = e.clientY;
    const ox0 = view.ox;
    const oy0 = view.oy;
    let engaged = e.button === 1;
    const move = (ev: MouseEvent): void => {
      const dx = ev.clientX - sx;
      const dy = ev.clientY - sy;
      if (!engaged && dx * dx + dy * dy > 16) {
        engaged = true;
        canvas.classList.add("panning");
      }
      if (engaged) {
        view.ox = ox0 + dx;
        view.oy = oy0 + dy;
        applyView();
      }
    };
    const up = (): void => {
      canvas.classList.remove("panning");
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  });

  const screenToWorld = (clientX: number, clientY: number): { x: number; y: number } => {
    const rect = canvas.getBoundingClientRect();
    return {
      x: (clientX - rect.left - view.ox) / view.z,
      y: (clientY - rect.top - view.oy) / view.z,
    };
  };

  // ─── frames ──────────────────────────────────────────────────────
  const frames = new Map<string, Frame>();

  const rectFromRec = (
    rec: Rec,
    defW: number,
    defH: number,
    idx: number,
  ): { x: number; y: number; w: number; h: number } => ({
    x: num(rec, "x") ?? 40 + (idx % 4) * 360,
    y: num(rec, "y") ?? 40 + Math.floor(idx / 4) * 280,
    w: num(rec, "width") ?? defW,
    h: num(rec, "height") ?? defH,
  });

  const persistRect = (aid: string, x?: number, y?: number, w?: number, h?: number): void => {
    // Geometry is FRONTEND state — patch the LOCAL record; the proxy_loader
    // debounce-persists it to the host web/kernel_state.
    const patch: Payload = {};
    if (x !== undefined) {
      patch["x"] = Math.round(x);
      patch["y"] = Math.round(y ?? 0);
    }
    if (w !== undefined) {
      patch["width"] = Math.round(w);
      patch["height"] = Math.round(h ?? 0);
    }
    kernel.updateMeta(aid, patch);
  };

  const startDrag = (e: MouseEvent, aid: string, el: HTMLElement): void => {
    e.preventDefault();
    el.classList.add("dragging");
    document.body.classList.add("dragging-frame");
    const sx = e.clientX;
    const sy = e.clientY;
    const ox = parseFloat(el.style.left);
    const oy = parseFloat(el.style.top);
    const move = (ev: MouseEvent): void => {
      el.style.left = `${ox + (ev.clientX - sx) / view.z}px`;
      el.style.top = `${oy + (ev.clientY - sy) / view.z}px`;
    };
    const up = (): void => {
      el.classList.remove("dragging");
      document.body.classList.remove("dragging-frame");
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      void persistRect(aid, parseFloat(el.style.left), parseFloat(el.style.top));
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  const startResize = (e: MouseEvent, aid: string, el: HTMLElement): void => {
    e.preventDefault();
    e.stopPropagation();
    el.classList.add("resizing");
    document.body.classList.add("dragging-frame");
    const sx = e.clientX;
    const sy = e.clientY;
    const ow = parseFloat(el.style.width);
    const oh = parseFloat(el.style.height);
    const move = (ev: MouseEvent): void => {
      el.style.width = `${Math.max(180, ow + (ev.clientX - sx) / view.z)}px`;
      el.style.height = `${Math.max(120, oh + (ev.clientY - sy) / view.z)}px`;
    };
    const up = (): void => {
      el.classList.remove("resizing");
      document.body.classList.remove("dragging-frame");
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      void persistRect(aid, undefined, undefined, parseFloat(el.style.width), parseFloat(el.style.height));
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  // Build the frame chrome (head + empty body + resize handle). Kind-agnostic:
  // the caller fills `.frame-body` (inline mount or iframe) and wires reload.
  const makeShell = (
    rec: Rec,
    title: string,
    rect: { x: number; y: number; w: number; h: number },
    onReload: () => void,
  ): { el: HTMLElement; body: HTMLElement; actions: HTMLElement } => {
    const el = document.createElement("div");
    el.className = "agent-frame";
    el.style.left = `${rect.x}px`;
    el.style.top = `${rect.y}px`;
    el.style.width = `${rect.w}px`;
    el.style.height = `${rect.h}px`;
    const locked = bool(rec, "delete_lock");
    el.innerHTML = `
      <div class="frame-head">
        <span class="id">${title} · <code style="opacity:0.6;font-size:10px">${rec.id}</code></span>
        <span class="actions">
          <span class="lock ${locked ? "locked" : ""}" title="${locked ? "unlock (allow delete)" : "lock (refuse delete)"}">${locked ? "🔒" : "🔓"}</span>
          <span class="reload" title="reload">⟳</span>
          <span class="close" title="delete">×</span>
        </span>
      </div>
      <div class="frame-body"></div>
      <div class="resize-handle"></div>
    `;
    world.appendChild(el);
    const head = el.querySelector(".frame-head") as HTMLElement;
    head.addEventListener("mousedown", (e) => {
      if ((e.target as Element).closest(".actions")) return;
      startDrag(e, rec.id, el);
    });
    (el.querySelector(".resize-handle") as HTMLElement).addEventListener("mousedown", (e) =>
      startResize(e as MouseEvent, rec.id, el),
    );
    (el.querySelector(".reload") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      onReload();
    });
    (el.querySelector(".lock") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      const cur = bool(frames.get(rec.id)?.rec ?? rec, "delete_lock");
      const lockEl = el.querySelector(".lock") as HTMLElement;
      lockEl.classList.toggle("locked", !cur);
      lockEl.textContent = !cur ? "🔒" : "🔓";
      kernel.updateMeta(rec.id, { delete_lock: !cur }); // local; proxy_loader syncs
    });
    (el.querySelector(".close") as HTMLElement).addEventListener("click", () => {
      const r = frames.get(rec.id)?.rec ?? rec;
      if (bool(r, "delete_lock")) {
        console.warn("[canvas] delete refused — delete_lock on", rec.id);
        return;
      }
      // If this view SPAWNED its host backend (owns_backend — e.g. the dblclick
      // terminal), delete that peer too so the pair is removed TOGETHER (the
      // backend's on_delete tears down its PTY/process). A view bound to a
      // PRE-EXISTING / shared backend leaves it running (weak peer — reopening
      // re-attaches).
      const backendId = str(r, "backend_id");
      if (bool(r, "owns_backend") && backendId) {
        void host.callHost("kernel", { type: "delete_agent", id: backendId });
      }
      kernel.remove(rec.id);
    });
    return {
      el,
      body: el.querySelector(".frame-body") as HTMLElement,
      actions: el.querySelector(".actions") as HTMLElement,
    };
  };

  // Mount a first-party TS view-agent inline into a fresh frame.
  const mountInline = async (rec: Rec, bundle: ViewBundle, idx: number): Promise<void> => {
    const rect = rectFromRec(rec, 480, 320, idx);
    let reload = (): void => {};
    const { el, body, actions } = makeShell(rec, str(rec, "display_name") ?? rec.id, rect, () => reload());
    const backend = str(rec, "backend_id") ?? rec.id; // weak peer ref to a host backend (or self)
    const handle = await bundle.mount({ kernel, mount: body, selfId: rec.id, backend, record: rec });
    frames.set(rec.id, { el, rec, kind: "inline", handle });
    // reload = remount the inline view in place
    reload = (): void => {
      handle.unmount();
      body.innerHTML = "";
      void Promise.resolve(bundle.mount({ kernel, mount: body, selfId: rec.id, backend, record: rec })).then((h) => {
        const f = frames.get(rec.id);
        if (f) f.handle = h;
      });
    };
    // declarative header chips, wired locally (no bus)
    for (const btn of handle.headerButtons ?? []) {
      const chip = document.createElement("span");
      chip.className = "ext";
      chip.textContent = btn.glyph;
      chip.title = btn.title;
      chip.addEventListener("click", (e) => {
        e.stopPropagation();
        const active = btn.onClick();
        if (btn.toggle) chip.classList.toggle("active", active === true);
      });
      actions.insertBefore(chip, actions.firstChild);
    }
  };

  // Mount untrusted/external content as an iframe.
  const mountIframe = (rec: Rec, wa: Webapp, idx: number): void => {
    const rect = rectFromRec(rec, wa.default_width ?? 340, wa.default_height ?? 240, idx);
    const { el, body } = makeShell(rec, wa.title ?? rec.id, rect, () =>
      host.emit(rec.id, { type: "reload_html" }),
    );
    const iframe = document.createElement("iframe");
    iframe.src = wa.url;
    body.appendChild(iframe);
    frames.set(rec.id, { el, rec, kind: "iframe" });
  };

  const removeFrame = (aid: string): void => {
    const f = frames.get(aid);
    if (f === undefined) return;
    if (f.kind === "inline" && f.handle) {
      try {
        f.handle.unmount();
      } catch (e) {
        console.error("[canvas] unmount raised:", e);
      }
    }
    f.el.remove();
    frames.delete(aid);
  };

  const applyRectFromRec = (el: HTMLElement, rec: Rec): void => {
    const x = num(rec, "x");
    if (x !== undefined) {
      el.style.left = `${x}px`;
      el.style.top = `${num(rec, "y") ?? 0}px`;
    }
    const w = num(rec, "width");
    if (w !== undefined) el.style.width = `${w}px`;
    const h = num(rec, "height");
    if (h !== undefined) el.style.height = `${h}px`;
  };

  const refresh = async (): Promise<void> => {
    // Members are this canvas's OWN children in the LOCAL tree (hydrated from
    // web/kernel_state, persisted back via proxy_loader). No host membership read.
    const root = kernel.rootId !== null ? kernel.get(kernel.rootId) : undefined;
    const members: Rec[] =
      root !== undefined
        ? [...root.children.values()].map((a) => a.record() as Rec)
        : [];

    let idx = 0;
    const seenDom = new Set<string>();
    const seenGl = new Set<string>();
    for (const rec of members) {
      const mid = rec.id;
      const existing = frames.get(mid);
      if (existing !== undefined) {
        existing.rec = rec;
        applyRectFromRec(existing.el, rec);
        seenDom.add(mid);
        if (glHost?.hasView(mid)) seenGl.add(mid);
        idx++;
        continue;
      }
      // Every member is a `*.ts` frontend bundle — match its handler_module to
      // an inline ViewBundle (terminal/chat fronts a backend; content agents
      // fall through to the probe below).
      const bundle = viewFor(rec["handler_module"]);
      if (bundle !== undefined) {
        await mountInline(rec, bundle, idx);
        seenDom.add(mid);
        idx++;
        continue;
      }
      // Otherwise probe the member's BACKEND peer (by id) for DOM/GL presence
      // — a content member (gl_agent) answers on its own id; a host-backed one
      // answers on its `backend_id`.
      const backend = str(rec, "backend_id") ?? mid;
      const [waR, glR] = await Promise.all([
        host.call(backend, { type: "get_webapp" }).catch(() => null),
        host.call(backend, { type: "get_gl_view" }).catch(() => null),
      ]);
      const wa = waR as Webapp | null;
      if (wa !== null && typeof wa.url === "string" && !("error" in (wa as object))) {
        mountIframe(rec, wa, idx);
        seenDom.add(mid);
        idx++;
      }
      const gl = glR as GlView | null;
      if (gl !== null && typeof gl.source === "string" && gl.source) {
        const gh = await ensureGlHost();
        if (!gh.hasView(mid)) gh.installView(mid, gl);
        seenGl.add(mid);
      }
    }
    for (const aid of [...frames.keys()]) if (!seenDom.has(aid)) removeFrame(aid);
    glHost?.prune(seenGl);
    const glCount = glHost === null ? 0 : seenGl.size;
    status.innerHTML = `canvas <code>${selfId}</code> · ${frames.size} dom · ${glCount} gl`;
  };

  // Re-render on LOCAL tree changes (added/updated/removed). The canvas owns
  // its membership; the proxy_loader syncs it to the host independently.
  kernel.addStateSubscriber(() => void refresh());

  // dblclick → spawn a terminal: create a host terminal_backend PEER (by id on
  // the host), then a LOCAL member that fronts it via the terminal view.
  canvas.addEventListener("dblclick", async (e) => {
    if ((e.target as Element).closest(".agent-frame")) return;
    const w = screenToWorld(e.clientX, e.clientY);
    // Discover a PTY-capable bundle from the live host catalog (no hardcoded
    // handler_module), then create it on the host root.
    const cat = (await host.callHost("kernel", { type: "reflect", bundles: "all" })) as {
      bundles?: Array<{ name?: string; handler_module?: string }>;
    };
    const pty = (cat?.bundles ?? []).find(
      (b) => PTY_HINT.test(b.handler_module ?? "") || PTY_HINT.test(b.name ?? ""),
    );
    if (!pty || typeof pty.handler_module !== "string") {
      console.warn("[canvas] no PTY-capable bundle in host catalog; skipping terminal spawn");
      return;
    }
    const created = (await host.callHost("kernel", {
      type: "create_agent",
      handler_module: pty.handler_module,
    })) as { id?: string; error?: string };
    if (!created || created.error || typeof created.id !== "string") {
      console.error("[canvas] backend create failed:", created);
      return;
    }
    kernel.register(
      new Agent({
        id: `term_${created.id}`,
        parentId: kernel.rootId ?? selfId,
        handlerModule: "terminal_view.ts", // a real frontend bundle; viewFor → terminalView
        meta: {
          backend_id: created.id, // weak peer ref to the host terminal_backend
          owns_backend: true, // dblclick spawned it → delete removes the pair together
          x: Math.round(w.x),
          y: Math.round(w.y),
        },
      }),
    );
  });

  await refresh();
}
