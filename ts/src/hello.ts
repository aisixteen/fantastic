import { Kernel } from "./kernel/kernel.ts";
import { Agent } from "./kernel/agent.ts";
import { WsBridge } from "./transport/bridge.ts";
import type { Payload } from "./kernel/json.ts";

// Starter entry — NO canvas. Proves the frontend kernel + bridge boot in the
// browser and federate to the live host: dial /fs_loader/ws, reflect the kernel,
// render the live agent tree. Served via the web/http alias method (an
// html_agent renders the mount page; a file agent serves this dist/) — no new
// host routes. The canvas view-agent layers on once this shape is approved.

const CSS = `
  :root { color-scheme: dark; }
  html,body { margin:0; height:100%; background:#06060c; color:#cdd6e4;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  #app { padding: 28px 32px; }
  h1 { font-size: 15px; font-weight: 600; letter-spacing:.5px; margin:0 0 4px;
    color:#e6ecf5; }
  h1 small { color:#6b7488; font-weight:400; }
  .status { font-size:12px; margin-bottom:20px; }
  .dot { display:inline-block; width:8px; height:8px; border-radius:50%;
    margin-right:7px; vertical-align:middle; background:#f7768e; }
  .dot.ok { background:#9ece6a; }
  ul { list-style:none; margin:0; padding-left:18px; border-left:1px solid rgba(255,255,255,.06); }
  li { padding:3px 0; font-size:13px; }
  .name { color:#e6ecf5; }
  code { color:#7aa2f7; background:rgba(122,162,247,.08); padding:1px 6px; border-radius:4px; font-size:11px; }
  .hm { color:#565f73; font-size:11px; }
  .card { background:rgba(15,15,25,.55); border:1px solid rgba(255,255,255,.08);
    border-radius:14px; padding:18px 22px; max-width:760px;
    box-shadow:0 8px 32px rgba(0,0,0,.4); backdrop-filter: blur(16px); }
`;

interface TreeNode {
  id: string;
  display_name?: string;
  handler_module?: string | null;
  children?: TreeNode[];
}

function renderTree(node: TreeNode): string {
  const name = node.display_name ?? node.id;
  const hm = node.handler_module ?? "(root)";
  const kids =
    node.children && node.children.length > 0
      ? `<ul>${node.children.map(renderTree).join("")}</ul>`
      : "";
  return `<li><span class="name">${name}</span> <code>${node.id}</code> <span class="hm">${hm}</span>${kids}</li>`;
}

const origin =
  (location.protocol === "https:" ? "wss://" : "ws://") + location.host;

const style = document.createElement("style");
style.textContent = CSS;
document.head.appendChild(style);
document.body.innerHTML =
  `<div id="app"><div class="card">` +
  `<h1>fantastic <small>· ts frontend kernel</small></h1>` +
  `<div class="status"><span class="dot" id="dot"></span><span id="msg">connecting…</span></div>` +
  `<ul id="tree"></ul></div></div>`;

const kernel = new Kernel();
kernel.setRoot(new Agent({ id: "frontend", sentence: "TS frontend kernel (starter)." }));
const bridge = new WsBridge(kernel, { origin, controlEndpoint: "fs_loader" });

try {
  const reply = (await kernel.send("kernel", {
    type: "reflect",
    tree: "all",
  } as Payload)) as { tree?: TreeNode; sentence?: string };
  const dot = document.getElementById("dot");
  if (dot) dot.classList.add("ok");
  const msg = document.getElementById("msg");
  if (msg) msg.textContent = `federated → ${origin}  ·  ${reply.sentence ?? ""}`;
  const tree = document.getElementById("tree");
  if (tree && reply.tree) tree.innerHTML = renderTree(reply.tree);
  // keep the socket warm; a later slice watches the tree live
  void bridge;
} catch (e) {
  const msg = document.getElementById("msg");
  if (msg) msg.textContent = `failed: ${(e as Error).message}`;
}
