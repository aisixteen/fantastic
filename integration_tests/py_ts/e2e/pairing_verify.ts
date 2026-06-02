// pairing_verify.ts — the PINNACLE assertion: did the readme-only builder agent
// derive the right PAIRING CARDINALITY from reflect + readmes ALONE?
//
//   terminal_view ↔ terminal_backend = 1:1 EXCLUSIVE (each view binds one backend
//     via backend_id; no backend shared by two views; no orphan backend).
//   ai_view ↔ AI backend = 1:1 (each view binds one AI backend via backend_id).
//   html_agent ↔ python_runtime = 1:N (panel has NO backend_id; its html body
//     send/watches a python_runtime by id — behavioral, non-exclusive).
//
// Reads HOST agents via reflect (backends live there) + FRONTEND view/panel records
// via the web_loader store (load_tree). Prints the derived structure, then asserts.
//
//   usage: node pairing_verify.ts <REST_URL>

const REST = process.argv[2];
if (!REST) {
  console.error("usage: node pairing_verify.ts <REST_URL>");
  process.exit(2);
}

type Rec = { id?: string; handler_module?: string; backend_id?: string; html?: string; children?: Rec[] };

async function getJSON(path: string): Promise<Rec> {
  const r = await fetch(REST + path);
  return (await r.json()) as Rec;
}
async function postJSON(id: string, payload: unknown): Promise<{ records?: Rec[] }> {
  const r = await fetch(`${REST}/${id}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
  return (await r.json()) as { records?: Rec[] };
}
function flatten(node: Rec | undefined, out: Rec[] = []): Rec[] {
  if (!node) return out;
  out.push(node);
  for (const c of node.children ?? []) flatten(c, out);
  return out;
}

const reflectRoot = await getJSON("/_reflect?tree=all");
// reflect returns { id, sentence, …, tree: { id, …, children:[…] } } — walk .tree.
const hostAgents = flatten((reflectRoot as { tree?: Rec }).tree ?? reflectRoot);
const frontend = (await postJSON("web_loader", { type: "load_tree" })).records ?? [];

const isAi = (hm: string | undefined) =>
  /(anthropic|ollama|nvidia_nim|foundation_models)_backend\.tools/.test(hm ?? "");
const termBackends = hostAgents.filter((a) => a.handler_module === "terminal_backend.tools");
const aiBackends = hostAgents.filter((a) => isAi(a.handler_module));
const pyRuntimes = hostAgents.filter((a) => a.handler_module === "python_runtime.tools");
const termViews = frontend.filter((r) => r.handler_module === "terminal_view.ts");
const aiViews = frontend.filter((r) => r.handler_module === "ai_view.ts");
const htmlPanels = frontend.filter((r) => r.handler_module === "html_agent.ts");

const fail: string[] = [];

console.log("=== what the builder created ===");
console.log(`  host backends:   ${termBackends.length} terminal · ${aiBackends.length} ai · ${pyRuntimes.length} python_runtime`);
console.log(`  frontend views:  ${termViews.length} terminal_view · ${aiViews.length} ai_view · ${htmlPanels.length} html_agent`);

// ── 1:1 terminal (exclusive) ──
const termBackendIds = new Set(termBackends.map((b) => b.id));
const termUsed = termViews.map((v) => v.backend_id).filter((x): x is string => !!x);
for (const v of termViews) {
  if (!v.backend_id) fail.push(`terminal_view ${v.id} has NO backend_id (expected 1:1)`);
  else if (!termBackendIds.has(v.backend_id)) fail.push(`terminal_view ${v.id} → missing backend ${v.backend_id}`);
}
if (new Set(termUsed).size !== termUsed.length) fail.push("a terminal_backend is shared by >1 view (must be exclusive 1:1)");
for (const b of termBackends) {
  const n = termViews.filter((v) => v.backend_id === b.id).length;
  if (n !== 1) fail.push(`terminal_backend ${b.id} fronted by ${n} views (must be exactly 1 — no orphan/shared)`);
}

// ── 1:1 ai ──
const aiBackendIds = new Set(aiBackends.map((b) => b.id));
for (const v of aiViews) {
  if (!v.backend_id) fail.push(`ai_view ${v.id} has NO backend_id (expected 1:1)`);
  else if (!aiBackendIds.has(v.backend_id)) fail.push(`ai_view ${v.id} → missing AI backend ${v.backend_id}`);
}

// ── 1:N html ↔ python_runtime (behavioral, no binding) ──
const pyIds = pyRuntimes.map((p) => p.id).filter((x): x is string => !!x);
let panelsWiredToExec = 0;
for (const p of htmlPanels) {
  if (p.backend_id) fail.push(`html_agent ${p.id} has a backend_id (${p.backend_id}) — should be 1:N (no exclusive binding)`);
  const body = typeof p.html === "string" ? p.html : "";
  if (pyIds.some((id) => body.includes(id)) || /python_runtime/.test(body)) panelsWiredToExec++;
}
if (pyRuntimes.length > 0 && panelsWiredToExec === 0) {
  fail.push("no html panel send/watches a python_runtime in its body (expected 1:N html↔exec wiring)");
}

console.log("=== cardinality verdict ===");
if (fail.length) {
  for (const f of fail) console.log("  ✗ " + f);
  console.log("FAIL: the builder did not derive the pairing cardinality from readmes alone");
  process.exit(1);
}
console.log("  ✓ terminal_view↔terminal_backend 1:1 exclusive");
console.log("  ✓ ai_view↔AI backend 1:1");
console.log(`  ✓ html_agent↔python_runtime 1:N (${panelsWiredToExec}/${htmlPanels.length} panels wired to a python_runtime, no backend_id)`);
console.log("PASS: the LLM derived the kernel pairing cardinality from readmes alone");
