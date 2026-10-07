import { Agent, Kernel, ProxyLoader, WsBridge, registerRamStore } from "./lib.ts";
import { GATEWAY, UserBridge, readToken, writeToken } from "./user_bridge.ts";
import { registerGlAgent } from "./bundles/gl_agent/gl_agent.ts";
import { registerHtmlAgent } from "./bundles/html_agent/html_agent.ts";
import { mountCanvas } from "./bundles/canvas/canvas.ts";
import { registerCanvas } from "./bundles/canvas/canvas_agent.ts";
import { registerBackground } from "./bundles/canvas/background_agent.ts";

// The canvas page. The frontend kernel boots as a PEER of the host: it dials the host's `web_loader` (by id) over the bridge (a host-side
// `kernel_state` rooted at `.fantastic/web/`, which the operator created — no
// automation), hydrates its OWN agent tree from there (`load_tree`), and
// persists every local change back (`proxy_loader`). The canvas renders the
// LOCAL member tree; host backends are weak peers referenced by id. One shared
// frontend tree, addressed by the loader's id — no per-session id.

const LOADER = "web_loader"; // the host-side web/kernel_state, by id
const origin =
  (location.protocol === "https:" ? "wss://" : "ws://") + location.host;

/** This app's bundles — the ones whose records this kernel runs locally.
 *  Anything else (e.g. a `*.tools` host record that strays in) weak-loads + skips.
 *  (The library registers its own memory store; the page never starts the
 *  kernel — an unstarted kernel keeps its execution rows in RAM, bounded by the
 *  ledger's ring; `kernel_start` is the operator's act.) */
function register(kernel: Kernel): void {
  registerGlAgent(kernel);
  registerHtmlAgent(kernel);
  registerCanvas(kernel); // the compositor root and its control plane
  registerBackground(kernel); // its singleton, delete-locked background (set_bg/clear_bg/get_bg)
  // View bundles that front a host backend are real frontend agents too — a
  // no-op handler makes their records load-able; the ViewBundle renders them.
  kernel.registerBundle("terminal_view.ts", () => null);
  kernel.registerBundle("ai_view.ts", () => null);
  kernel.registerBundle("reflect_view.ts", () => null);
  kernel.registerBundle("setup_view.ts", () => null);
  kernel.registerBundle("stream_view.ts", () => null);

  // Each frontend bundle declares its role via reflect (readme=true): the view
  // CLIENTS name the host CAPABILITY + verb surface they front (an LLM weaves the
  // pairing from this + the host's capability readme — no hardcoded pairing);
  // content agents say what they hold. Direction is frontend→host: the JS knows
  // it fronts a host capability; the host stays ignorant of the frontend.
  kernel.setBundleReadme(
    "html_agent.ts",
    "An html cell: its record's `html` body, rendered in a sandboxed frame (unsandboxed when trusted). Verbs: get_html, set_html {bytes}. A PANEL'S JS (an html cell's body) has `fantastic`: `await fantastic.send(id, payload)` → the reply · `fantastic.watch(id, ev => …)` → events (returns off) · `fantastic.emit(id, payload)` · `fantastic.onMessage(p => …)`. Its calls are judged by the CELL's can_call — grant what it calls (`{<id>: [verbs…, watch]}`); a reply's `grants_missing` names any it lacks. On the host they arrive as `user`: a host agent you create for a panel needs `callable_by: {user: [<the verbs the panel calls>, watch]}` in its create_agent (a new agent admits no one); never drop `user` from an existing agent's callable_by. Place, move, grant and remove cells through the view (`view_root`).",
  );
  kernel.setBundleReadme(
    "gl_agent.ts",
    "Frontend WebGL content agent. Renders its record's `gl_source` shader and reacts to events it watches by id. Content, not a host client.",
  );
  kernel.setBundleReadme(
    "terminal_view.ts",
    "A terminal cell: an xterm onto a host exec agent's PTY job (`backend_id`) — it starts the job, writes keystrokes (write), resizes, and draws its output.",
  );
  kernel.setBundleReadme(
    "ai_view.ts",
    "A chat cell: the human's view of a host AI (`backend_id`). It sends the human's turns (send), draws the AI's streamed parts, and stops a turn (stop).",
  );
}

/** Fresh (the host namespace holds no records): seed the canvas root — the
 *  operator's first conscious agent. `boot` persists it with the first write. */
function seed(kernel: Kernel): void {
  kernel.setRoot(
    new Agent({
      id: "canvas",
      handlerModule: "canvas.ts",
      sentence: "The canvas — the page's root; its views are under it.",
    }),
  );
}

// THE BOOT, the library's `boot` composed here with one change: every host call
// goes through `user` (`user_bridge.ts`).
const kernel = new Kernel();
registerRamStore(kernel);
register(kernel);
const inner = new WsBridge(kernel, { origin, controlEndpoint: GATEWAY });
// the token is read ONCE, at load: a token typed in another tab takes effect here
// on this tab's next load, never mid-session
const token = readToken();
new UserBridge(kernel, inner, { token: () => token });

// IS THIS TAB LET IN: `user.forward` to the host kernel's reflect — refused
// only when `user` carries a token this tab does not give. Refused → ask for
// a token, and nothing else runs.
const me = (await kernel.callHost("kernel", { type: "reflect", tree: "none" } as never)) as Record<string, unknown>;
if (me["reason"] === "unauthorized") {
  askToken(String(me["error"] ?? "no answer"));
  await new Promise(() => {}); // the form reloads the page
}

const loader = new ProxyLoader(kernel, LOADER);
kernel.persistenceProvider = LOADER;
const records = await loader.loadTree();
if (records.length > 0) kernel.load(records);
else seed(kernel);
inner.announce();
loader.start();

// THE ROOT'S BOOT MINTS THE GUID (once; it persists with the root record) — the
// canvas's identity as a PEER kernel: the bridge says `hello {guid}` with it, and
// the host's agents address this canvas as `<guid>|canvas`. `boot` hydrates the
// tree but boots no agent, so the page boots its own root.
if (kernel.rootId !== null) await kernel.send([{ target: kernel.rootId, payload: { type: "boot" } }]);

await mountCanvas({
  kernel,
  mount: document.body,
  selfId: kernel.rootId ?? "canvas",
  flush: () => loader.flush(),
});

/** The token form: shown when the host refuses this tab. The token goes to
 *  browser storage only, then the page reloads. */
function askToken(why: string): void {
  const box = document.createElement("form");
  box.className = "token-ask";
  box.innerHTML = `<p class="why"></p><input type="password" autocomplete="off" placeholder="token"><button>open</button>`;
  (box.querySelector(".why") as HTMLElement).textContent = `This canvas wants a token — ${why}.`;
  box.style.cssText = "position:fixed;inset:0;display:grid;place-content:center;gap:10px;font:14px system-ui;color:#cde;background:#0b0b10";
  box.addEventListener("submit", (e) => {
    e.preventDefault();
    writeToken((box.querySelector("input") as HTMLInputElement).value.trim());
    location.reload();
  });
  document.body.replaceChildren(box);
  (box.querySelector("input") as HTMLInputElement).focus();
}
