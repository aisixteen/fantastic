import { Kernel } from "./kernel/kernel.ts";
import { Agent } from "./kernel/agent.ts";
import { WsBridge } from "./transport/bridge.ts";
import { ProxyLoader } from "./bundles/loader/proxy_loader.ts";
import { registerGlAgent } from "./bundles/gl_agent/gl_agent.ts";
import { registerHtmlAgent } from "./bundles/html_agent/html_agent.ts";
import { mountCanvas } from "./bundles/canvas/canvas.ts";

// The frontend kernel boots as a PEER of the host. It dials the host's
// `web_loader` alias over the bridge (a host-side `kernel_state` rooted at
// `.fantastic/web/`, which the operator created — no automation), hydrates its
// OWN agent tree from there (`load_tree`), and persists every local change back
// (`proxy_loader`). The canvas renders the LOCAL member tree; host backends are
// weak peers referenced by id. One shared frontend tree, addressed by alias —
// no per-session id.

const LOADER = "web_loader"; // the host-side web/kernel_state alias
const origin =
  (location.protocol === "https:" ? "wss://" : "ws://") + location.host;

const kernel = new Kernel();
// Register the JS bundles whose records this kernel runs locally. Anything
// else (e.g. a `*.tools` host record that strays in) weak-loads + skips.
registerGlAgent(kernel);
registerHtmlAgent(kernel);
kernel.registerBundle("canvas.ts", () => null); // the compositor root (rendered, not a verb agent)
// View bundles that front a host backend are real frontend agents too — a
// no-op handler makes their records load-able; the ViewBundle renders them.
kernel.registerBundle("terminal_view.ts", () => null);
kernel.registerBundle("ai_view.ts", () => null);

// Each frontend bundle declares its role via reflect (readme=true): the view
// CLIENTS name the host CAPABILITY + verb surface they front (an LLM weaves the
// pairing from this + the host's capability readme — no hardcoded pairing);
// content agents say what they hold. Direction is frontend→host: the JS knows
// it fronts a host capability; the host stays ignorant of the frontend.
kernel.setBundleReadme(
  "canvas.ts",
  "Canvas compositor (frontend root). Renders the frontend's own member tree; mounts each member inline via its view bundle, or as an iframe for external content. Not a host client.",
);
kernel.setBundleReadme(
  "html_agent.ts",
  "Frontend HTML content agent. Holds a mutable `html` body in its record, rendered in a sandboxed frame; the injected connector relays send/emit/watch/onMessage to the kernel. Content, not a host client.",
);
kernel.setBundleReadme(
  "gl_agent.ts",
  "Frontend WebGL content agent. Renders its record's `gl_source` shader and reacts to events it watches by id. Content, not a host client.",
);
kernel.setBundleReadme(
  "terminal_view.ts",
  "HTML/xterm CLIENT for a host PTY. Fronts any agent answering the PTY verb surface (boot/write/ack/resize/interrupt/stop) and emitting output/exited, bound by `backend_id`: watches the backend's output, renders it, sends keystrokes via write.",
);
kernel.setBundleReadme(
  "ai_view.ts",
  "HTML chat CLIENT for a host LLM backend. Fronts any agent answering send/history/interrupt/status, bound by `backend_id`: renders streamed token/done events, sends user turns via send.",
);

new WsBridge(kernel, { origin, controlEndpoint: LOADER });
// The ONE auto-added agent in the JS runtime — its single autoagent (the loader),
// mirroring the host root loader. Everything else below is explicit.
const loader = new ProxyLoader(kernel, LOADER);
// Surface the persistence provider on the root reflect (`persistence:{provider}`):
// the frontend persists records THROUGH the host loader it proxies to.
kernel.persistenceProvider = LOADER;

const records = await loader.loadTree();
if (records.length === 0) {
  // Fresh: seed the canvas root — the operator's first conscious agent.
  kernel.setRoot(
    new Agent({
      id: "canvas",
      handlerModule: "canvas.ts",
      sentence: "Canvas compositor — renders the frontend's own member tree.",
    }),
  );
} else {
  kernel.load(records);
}
loader.start(); // watch the local tree → persist/forget over the bridge

await mountCanvas({ kernel, mount: document.body, selfId: kernel.rootId ?? "canvas" });
