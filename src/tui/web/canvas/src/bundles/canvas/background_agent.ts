import { str } from "../../lib.ts";
import { bodyText } from "../body_text.ts";
import type { Handler, Json } from "../../lib.ts";

// THE BACKGROUND — its own agent on the canvas's surface, a SINGLETON (owner,
// 2026-09-30): exactly one per canvas, a child of the canvas root, created by
// the canvas when missing; it cannot be closed or deleted (`delete_lock: true`
// on its record — the kernel refuses `delete_agent` on it, or on any ancestor
// of it), only updated. Its one piece of state is `bg_source`: a GL source (the
// same THREE/GLSL text a gl_agent holds), rendered behind every member and
// swapped in place when it changes. It persists through the ws loader.

export const BACKGROUND = "background.ts";

/** The background's id on every canvas. */
export const BACKGROUND_ID = "background";

/** How a GL source runs (the GL host's contract, `gl_host.ts`). */
export const GL_SIGNATURE = ["THREE", "scene", "t", "onFrame", "cleanup"] as const;

export const BACKGROUND_README =
  "The canvas's BACKGROUND — a singleton: one per canvas, delete-locked (it can only be updated). " +
  "`set_bg {bytes}` replaces it: `bytes` is JAVASCRIPT, verbatim, run as " +
  "`new Function(\"THREE\", \"scene\", \"t\", \"onFrame\", \"cleanup\", bytes)` in the canvas's shared three.js scene, " +
  "behind every cell — NOT CSS, NOT GLSL alone. `THREE` is three.js, `scene` the scene to add meshes to, `t` the " +
  "time, `onFrame(fn)` registers a per-frame callback `fn(t)`, `cleanup(fn)` runs on replace. Example — a slow blue " +
  "gradient: `const m = new THREE.ShaderMaterial({uniforms:{u:{value:0}}, vertexShader:'void main(){gl_Position=vec4(position.xy,0.,1.);}', " +
  "fragmentShader:'uniform float u; void main(){vec2 p=gl_FragCoord.xy/vec2(1600.,900.); gl_FragColor=vec4(0.05,0.1+0.1*p.y,0.3+0.2*sin(u*0.2+p.x),1.);}'}); " +
  "scene.add(new THREE.Mesh(new THREE.PlaneGeometry(2,2), m)); onFrame(t => m.uniforms.u.value = t);`. " +
  "A source that does not compile is refused, with the error. `clear_bg` removes it, `get_bg` returns it; it persists with its record.";

export const backgroundAgent: Handler = (id, payload, kernel): Json => {
  const agent = kernel.get(id);
  const bg = (): string => {
    const s = agent?.meta["bg_source"];
    return typeof s === "string" ? s : "";
  };
  switch (str(payload, "type")) {
    case "set_bg": {
      // the body is `bytes` (verbatim); `source` names it too
      const source = bodyText(payload["bytes"] ?? payload["source"]);
      if (source === null || source.trim() === "") {
        return { error: "background: set_bg needs the GL source as a non-empty string (`bytes`, verbatim)", reason: "bad_arg", arg: "bytes" };
      }
      // it must COMPILE as the GL host will run it — refused here, with the
      // error, rather than failing later where nobody sees it
      try {
        new Function(...GL_SIGNATURE, source);
      } catch (e) {
        return {
          error: `background: set_bg — the source does not compile: ${(e as Error).message}`,
          reason: "bad_arg",
          arg: "bytes",
          hint: `bytes is JavaScript run as new Function(${GL_SIGNATURE.map((x) => JSON.stringify(x)).join(", ")}, bytes) — reflect the background for an example`,
        };
      }
      kernel.updateMeta(id, { bg_source: source }); // → updated → persist + re-render
      return { ok: true, bytes: source.length };
    }
    case "clear_bg":
      kernel.updateMeta(id, { bg_source: "" });
      return { ok: true };
    case "get_bg":
      return { source: bg() };
    // The GL host's contract (`GlView`), so the background mounts like any GL view.
    case "get_gl_view":
      return { id, source: bg() };
    case "boot":
    case "shutdown":
      return null;
    default:
      return { error: `background: unknown verb '${str(payload, "type")}'`, reason: "unhandled" };
  }
};

/** Register the background's bundle. */
export function registerBackground(kernel: {
  registerBundle(hm: string, h: Handler): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(BACKGROUND, backgroundAgent);
  kernel.setBundleReadme(BACKGROUND, BACKGROUND_README);
}
