import { Host } from "../host.ts";

// The canvas WebGL host — ported from canvas_webapp/index.html's GL scaffolding.
// A shared THREE renderer/scene/camera (locked to the canvas pan/zoom) hosting
// one THREE.Group per GL member. Each member's `get_gl_view` source runs in its
// own group via new Function(THREE, scene, t, onFrame, cleanup) — `scene` is
// the per-view group (the GL analogue of an iframe), so a view can be disposed +
// recompiled in place without touching siblings. The source `t` is a shim over
// the bridge (call/emit/subscribeState) so backend GL sources (telemetry_pane,
// operator gl_agents) run unchanged. three is vendored + lazily imported.

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
  selfId: string;
  fetchSource: (agentId: string) => Promise<GlView | null>;
}): Promise<GlHost> {
  const { bg, view, host, selfId, fetchSource } = opts;
  const THREE = (await import("three")) as Record<string, unknown> & {
    WebGLRenderer: new (o: unknown) => any;
    Scene: new () => any;
    PerspectiveCamera: new (...a: number[]) => any;
    Group: new () => any;
  };

  const renderer = new THREE.WebGLRenderer({ canvas: bg, antialias: false, alpha: false });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
  renderer.setClearColor(0x06060c, 1);
  renderer.setSize(window.innerWidth, window.innerHeight, false);

  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(60, window.innerWidth / window.innerHeight, 0.1, 2000);
  camera.position.set(0, BASE_Y, BASE_Z);
  camera.lookAt(0, LOOK_Y, 0);

  const views = new Map<string, ViewEntry>();
  const frameCbs = new Map<string, Set<(t: number) => void>>();

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
    const cbs = new Set<(t: number) => void>();
    frameCbs.set(agentId, cbs);
    try {
      // eslint-disable-next-line @typescript-eslint/no-implied-eval
      const fn = new Function("THREE", "scene", "t", "onFrame", "cleanup", gv.source) as (
        three: unknown,
        sceneGroup: unknown,
        t: unknown,
        onFrame: (cb: (time: number) => void) => void,
        cleanupArr: Array<() => void>,
      ) => void;
      fn(THREE, group, sourceT, (cb) => cbs.add(cb), cleanup);
      const entry: ViewEntry = { group, cleanup };
      views.set(agentId, entry);
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
  }

  function applyCamera(): void {
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
  let raf = 0;
  function frame(): void {
    const time = (performance.now() - start) / 1000;
    for (const cbs of frameCbs.values()) {
      for (const cb of cbs) {
        try {
          cb(time);
        } catch (e) {
          console.error("[gl] onFrame raised:", e);
        }
      }
    }
    renderer.render(scene, camera);
    raf = requestAnimationFrame(frame);
  }
  raf = requestAnimationFrame(frame);

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
    resize() {
      camera.aspect = window.innerWidth / window.innerHeight;
      renderer.setSize(window.innerWidth, window.innerHeight, false);
      applyCamera();
    },
    dispose() {
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
