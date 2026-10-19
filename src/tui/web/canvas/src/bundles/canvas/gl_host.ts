import { Host } from "../../lib.ts";
import type { Kernel, Payload } from "../../lib.ts";
import { onSkin, skinNow } from "../skin/apply.ts";
import type { ColorValue } from "../skin/tokens.ts";

// The canvas WebGL host — ported from canvas_webapp/index.html's GL scaffolding.
// A shared THREE renderer/scene/camera (locked to the canvas pan/zoom) hosting
// one THREE.Group per GL member. Each member's `get_gl_view` source runs in its
// own group via new Function(THREE, scene, t, onFrame, cleanup) — `scene` is
// the per-view group (the GL analogue of an iframe), so a view can be disposed +
// recompiled in place without touching siblings. The source `t` is a shim over
// the bridge (call/emit/subscribeState) so backend GL sources (telemetry_pane,
// operator gl_agents) run unchanged. three is vendored + lazily imported.
//
// Each view's `t` is ITS OWN, like an html panel's connector: `t.agent_id` is
// the view's id, `t.emit(target, payload)` goes out AS THE VIEW (a local agent
// gets it through the page kernel, gated by the view's `can_call` and the
// target's `callable_by`; anything else crosses the bridge), and
// `t.onMessage(cb)` receives what is emitted TO the view.

export interface GlView {
  source: string;
}
export interface GlHost {
  installView(agentId: string, view: GlView): void;
  updateView(agentId: string): Promise<void>;
  removeView(agentId: string): void;
  hasView(agentId: string): boolean;
  /** remove every GL view whose id isn't in `keep`. */
  prune(keep: Set<string>): void;
  applyCamera(): void;
  /** resolves once the next frame is drawn (a just-installed view's shaders
   *  compile in it) */
  rendered(): Promise<void>;
  resize(): void;
  dispose(): void;
}

interface ViewEntry {
  group: unknown; // THREE.Group
  cleanup: Array<() => void>;
}

const BASE_Z = 140;
const BASE_Y = 14;
const LOOK_Y = -2;

export async function createGlHost(opts: {
  bg: HTMLCanvasElement;
  view: { z: number; ox: number; oy: number };
  host: Host;
  /** the page kernel: each view emits as itself and hears its own inbox */
  kernel: Kernel;
  selfId: string;
  fetchSource: (agentId: string) => Promise<GlView | null>;
}): Promise<GlHost> {
  const { bg, view, host, kernel, selfId, fetchSource } = opts;
  const THREE = (await import("three")) as Record<string, unknown> & {
    WebGLRenderer: new (o: unknown) => any;
    Scene: new () => any;
    PerspectiveCamera: new (...a: number[]) => any;
    Group: new () => any;
  };

  // ALPHA ON, for every skin (a context attribute, set once): under a native
  // backdrop the page's colour is transparent and the window's material shows
  const renderer = new THREE.WebGLRenderer({ canvas: bg, antialias: false, alpha: true });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
  const clearFromSkin = (): void => {
    const c = skinNow().skin.values.get("color.page.bg") as ColorValue;
    renderer.setClearColor(c.hex, c.alpha);
  };
  clearFromSkin();
  renderer.setSize(window.innerWidth, window.innerHeight, false);

  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(60, window.innerWidth / window.innerHeight, 0.1, 2000);
  camera.position.set(0, BASE_Y, BASE_Z);
  camera.lookAt(0, LOOK_Y, 0);

  const views = new Map<string, ViewEntry>();
  // A view's per-frame callbacks. `hz` absent: every frame (a GL cell animating
  // freely — the frame is drawn each time); `hz` given: a CLOCK ticked at most
  // that often (a background drifting slowly) — DRAWN ONLY WHEN DUE (owner
  // 2026-10-12: the empty canvas held the GPU at 20→70% repainting near-identical
  // frames), and on any frame drawn anyway (a pan, a zoom) so it stays current.
  interface FrameCb { fn: (t: number) => void; hz?: number; last: number }
  const frameCbs = new Map<string, Set<FrameCb>>();
  // something changed that the next frame must show: the camera, the size, a view
  let dirty = true;
  // the frame loop: 0 when stopped (nothing to draw); `wake` starts it again
  let raf = 0;
  function wake(): void {
    if (raf === 0) raf = requestAnimationFrame(frame);
  }
  // the skin changes: the clear colour follows, drawn on the next frame
  const offSkin = onSkin(() => {
    clearFromSkin();
    dirty = true;
    wake();
  });

  // a minimal transport shim for GL sources (call/emit/subscribeState + a
  // local bus). The old `t.on`/`t.watch` global-dispatch isn't used by the
  // backend GL sources (telemetry drives off subscribeState); covered if needed.
  const localBusListeners = new Map<string, Set<(d: unknown) => void>>();
  const sourceT = {
    agent_id: selfId,
    call: (target: string, payload: Record<string, unknown>) => host.call(target, payload as never),
    emit: (target: string, payload: Record<string, unknown>) => host.emit(target, payload as never),
    subscribeState: (handler: (frame: Record<string, unknown>) => void) => host.subscribeState(handler),
    bus: {
      broadcast: (d: { type?: string }) => {
        const ls = d.type ? localBusListeners.get(d.type) : undefined;
        if (ls) for (const l of ls) l(d);
      },
      send: (_target: string, d: { type?: string }) => sourceT.bus.broadcast(d),
      on: (type: string, handler: (d: unknown) => void) => {
        const set = localBusListeners.get(type) ?? new Set();
        set.add(handler);
        localBusListeners.set(type, set);
        return () => set.delete(handler);
      },
    },
  };

  function disposeObject3D(o: any): void {
    o.traverse((n: any) => {
      if (n.geometry) {
        try {
          n.geometry.dispose();
        } catch {
          /* idempotent */
        }
      }
      const mats = Array.isArray(n.material) ? n.material : [n.material];
      for (const m of mats) {
        if (!m) continue;
        for (const k in m) {
          const v = m[k];
          if (v && v.isTexture) {
            try {
              v.dispose();
            } catch {
              /* idempotent */
            }
          }
        }
        try {
          m.dispose();
        } catch {
          /* idempotent */
        }
      }
    });
  }

  function mountSource(agentId: string, gv: GlView): ViewEntry | null {
    const group = new THREE.Group();
    scene.add(group);
    const cleanup: Array<() => void> = [];
    const cbs = new Set<FrameCb>();
    frameCbs.set(agentId, cbs);
    try {
      // eslint-disable-next-line @typescript-eslint/no-implied-eval
      const fn = new Function("THREE", "scene", "t", "onFrame", "cleanup", gv.source) as (
        three: unknown,
        sceneGroup: unknown,
        t: unknown,
        onFrame: (cb: (time: number) => void, opt?: { hz?: number }) => void,
        cleanupArr: Array<() => void>,
      ) => void;
      const t = {
        ...sourceT,
        agent_id: agentId,
        // AS THE AGENT: through its own handler (`panel_relay.ts`) — gated by
        // its `can_call`, dropped when denied
        emit: (target: string, payload: Record<string, unknown>): void => {
          void kernel.send([{ target: agentId, payload: { type: "panel_emit", target, payload } as unknown as Payload }]);
        },
        onMessage: (cb: (payload: Payload) => void): (() => void) => {
          const off = kernel.onInbox(agentId, cb);
          cleanup.push(off);
          return off;
        },
      };
      fn(THREE, group, t, (cb, opt) => {
        const hz = typeof opt?.hz === "number" && opt.hz > 0 ? opt.hz : undefined;
        cbs.add({ fn: cb, hz, last: -Infinity });
      }, cleanup);
      const entry: ViewEntry = { group, cleanup };
      views.set(agentId, entry);
      dirty = true;
      wake();
      return entry;
    } catch (e) {
      console.error("[gl] mount failed for", agentId, ":", e);
      scene.remove(group);
      disposeObject3D(group);
      frameCbs.delete(agentId);
      return null;
    }
  }

  function teardown(agentId: string): void {
    const entry = views.get(agentId);
    if (!entry) return;
    for (const fn of entry.cleanup) {
      try {
        fn();
      } catch (e) {
        console.error("[gl] cleanup raised:", e);
      }
    }
    scene.remove(entry.group);
    disposeObject3D(entry.group);
    views.delete(agentId);
    frameCbs.delete(agentId);
    dirty = true;
    wake(); // one more frame clears what it drew
  }

  function applyCamera(): void {
    dirty = true;
    wake();
    const fovRad = (camera.fov * Math.PI) / 180;
    const worldH = 2 * BASE_Z * Math.tan(fovRad / 2);
    const W = window.innerWidth;
    const H = window.innerHeight;
    const wpp = worldH / H;
    const cwX = (W / 2 - view.ox) / view.z;
    const cwY = (H / 2 - view.oy) / view.z;
    const lookX = (cwX - W / 2) * wpp;
    const lookY = -(cwY - H / 2) * wpp + LOOK_Y;
    camera.position.set(lookX, lookY + (BASE_Y - LOOK_Y), BASE_Z);
    camera.zoom = view.z;
    camera.updateProjectionMatrix();
    camera.lookAt(lookX, lookY, 0);
  }
  applyCamera();

  const start = performance.now();
  const drawn: Array<() => void> = []; // waiting for the next drawn frame
  function frame(): void {
    const time = (performance.now() - start) / 1000;
    // draw THIS frame? when something changed, a free-running view, a clock due,
    // or a caller waiting for a drawn frame
    let draw = dirty || drawn.length > 0;
    for (const cbs of frameCbs.values()) {
      for (const cb of cbs) if (cb.hz === undefined || time - cb.last >= 1 / cb.hz - 1e-3) draw = true;
    }
    if (draw) {
      for (const cbs of frameCbs.values()) {
        for (const cb of cbs) {
          cb.last = time;
          try {
            cb.fn(time);
          } catch (e) {
            console.error("[gl] onFrame raised:", e);
          }
        }
      }
      renderer.render(scene, camera);
      dirty = false;
      for (const r of drawn.splice(0)) r();
    }
    // NOTHING TO DRAW (no GL view — the background set to none): the last frame
    // just cleared the scene, and the loop STOPS — no GPU work for an empty
    // canvas; a view installed again starts it (`wake`). A frame not drawn costs
    // nothing: no draw, so the browser keeps the last one on screen.
    raf = views.size === 0 && !dirty ? 0 : requestAnimationFrame(frame);
  }
  wake(); // one loop only: applyCamera() above may have started it

  const api: GlHost = {
    hasView: (agentId) => views.has(agentId),
    installView(agentId, gv) {
      if (views.has(agentId)) return;
      if (!gv || typeof gv.source !== "string" || !gv.source) return;
      if (mountSource(agentId, gv)) host.on(agentId, "gl_source_changed", () => void api.updateView(agentId));
    },
    async updateView(agentId) {
      if (!views.has(agentId)) return;
      const gv = await fetchSource(agentId);
      if (!gv || typeof gv.source !== "string" || !gv.source) return;
      teardown(agentId);
      mountSource(agentId, gv);
    },
    removeView(agentId) {
      if (!views.has(agentId)) return;
      teardown(agentId);
      host.unwatch(agentId);
    },
    prune(keep) {
      for (const id of [...views.keys()]) {
        if (!keep.has(id)) {
          teardown(id);
          host.unwatch(id);
        }
      }
    },
    applyCamera,
    rendered: () =>
      new Promise<void>((res) => {
        drawn.push(res);
        wake();
      }),
    resize() {
      camera.aspect = window.innerWidth / window.innerHeight;
      renderer.setSize(window.innerWidth, window.innerHeight, false);
      applyCamera(); // marks the frame dirty
    },
    dispose() {
      offSkin();
      cancelAnimationFrame(raf);
      for (const id of [...views.keys()]) teardown(id);
      try {
        renderer.dispose();
      } catch {
        /* best effort */
      }
    },
  };
  return api;
}
