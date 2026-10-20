// TIPS & TRICKS (owner 2026-10-09) — a PLAIN HTML CELL (`html_agent.ts`) whose
// default body is this card: macOS Tips style in the canvas palette, one tip at
// a time, ‹ › / ← → / the dots page it. It is the cell's own `html`, so it is
// EDITED like any html cell on the running canvas — its `set_html`, the view's
// `update {id, bytes}`, or by asking an AI ("add a tip about …"): the tips are
// the `TIPS` list in its script. In a body, `[[key]]` draws a keycap and
// `code` sets mono.

export const TIPS_HTML = `<style>
  html, body { margin: 0; height: 100%; background: transparent; }
  .tips { height: 100%; box-sizing: border-box; display: flex; flex-direction: column; padding: 18px 20px 14px;
    font: 13px/1.5 -apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, sans-serif; color: #e8e6f0;
    background: linear-gradient(160deg, rgba(40,34,62,0.92), rgba(18,16,28,0.92)); outline: none; }
  .kicker { font-size: 10.5px; font-weight: 600; letter-spacing: .14em; text-transform: uppercase; color: rgba(220,215,240,0.5); }
  .hero { display: flex; gap: 14px; align-items: center; margin: 12px 0 10px; }
  .glyph { flex: none; width: 52px; height: 52px; border-radius: 14px; display: flex; align-items: center; justify-content: center;
    color: #c9a8ff; background: radial-gradient(circle at 30% 25%, rgba(183,140,255,0.38), rgba(110,80,200,0.14));
    box-shadow: inset 0 1px 0 rgba(255,255,255,0.14), 0 6px 18px -6px rgba(120,80,220,0.55); }
  h2 { margin: 0; font-size: 17px; font-weight: 600; letter-spacing: -0.01em; color: #fff; }
  .body { flex: 1; min-height: 0; overflow: auto; color: rgba(232,230,240,0.86); }
  kbd { display: inline-block; min-width: 18px; padding: 0 5px; margin: 0 1px; border-radius: 5px; text-align: center;
    font: 600 11px/18px -apple-system, system-ui, sans-serif; color: #f2f0fa; background: linear-gradient(#3a3550, #2a2640);
    box-shadow: inset 0 -1px 0 rgba(0,0,0,0.45), inset 0 1px 0 rgba(255,255,255,0.10), 0 0 0 0.5px rgba(255,255,255,0.12); }
  code { font: 11.5px ui-monospace, SFMono-Regular, Menlo, monospace; color: #c9a8ff; background: rgba(183,140,255,0.10); padding: 1px 4px; border-radius: 4px; }
  .nav { display: flex; align-items: center; justify-content: space-between; margin-top: 12px; }
  .arrows { display: flex; gap: 8px; }
  .arrow { all: unset; cursor: pointer; width: 28px; height: 28px; border-radius: 50%; display: flex; align-items: center; justify-content: center;
    color: #e8e6f0; background: rgba(255,255,255,0.07); box-shadow: inset 0 0 0 0.5px rgba(255,255,255,0.14); }
  .arrow:hover { background: rgba(255,255,255,0.14); }
  .dots { display: flex; gap: 6px; }
  .dot { all: unset; cursor: pointer; width: 6px; height: 6px; border-radius: 50%; background: rgba(255,255,255,0.22); }
  .dot.on { background: #c9a8ff; width: 16px; border-radius: 3px; }
  .count { font-size: 11px; color: rgba(220,215,240,0.45); min-width: 44px; text-align: right; }
</style>
<div class="tips" id="card" tabindex="0">
  <div class="kicker">Tips &amp; tricks</div>
  <div class="hero">
    <div class="glyph"><svg viewBox="0 0 24 24" width="28" height="28" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l1.8 4.7L18.5 9.5l-4.7 1.8L12 16l-1.8-4.7L5.5 9.5l4.7-1.8z"/><path d="M18 15l.8 2.2L21 18l-2.2.8L18 21l-.8-2.2L15 18l2.2-.8z"/></svg></div>
    <h2 id="title"></h2>
  </div>
  <div class="body" id="body"></div>
  <div class="nav">
    <div class="arrows">
      <button class="arrow" id="prev" title="previous tip"><svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 5l-7 7 7 7"/></svg></button>
      <button class="arrow" id="next" title="next tip"><svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 5l7 7-7 7"/></svg></button>
    </div>
    <div class="dots" id="dots"></div>
    <span class="count" id="count"></span>
  </div>
</div>
<script>
// THE TIPS — edit this list (here, or ask your AI to): {title, body}; in a body
// [[key]] draws a keycap and \`code\` sets mono
const TIPS = [
  { title: "Welcome to Fantastic Canvas", body: "A shared space where AI agents and humans work as equals, on an Erlang-like execution environment. To see how it works: double-click empty canvas, start your coding agent and ask it to \`lookaround\` — it reads the agentic readmes in \`.fantastic/\`. Then ask it to draw a diagram of the environment: it appears right here on the canvas." },
  { title: "Add bundled agents", body: "Double-click empty canvas to place more bundled agents: \`watch\` shows the message flow under the hood, the file tree shows the project (every agent is a folder in it) and the preview shows or edits what you select, and \`backgrounds\` switches to a stock background or keeps your own list. Ask your coding agent to add one — it's fun." },
  { title: "Make your own bundled agent", body: "Say you want a dashboard: ask your coding agent to create a Python agent that reads your project's code and serves an interactive dashboard. Then ask it to bundle that agent, and you can place it from the double-click popup in this project." },
  { title: "Run a terminal", body: "Add a terminal window to run terminal programs from the project's folder, its PTY streamed here. Try it: double-click empty canvas and pick \`terminal\` — it starts a real PTY in the project folder." },
  { title: "Try the Fantastic AI agent", body: "Fantastic has its own AI agent. It isn't as polished as the assistants on the market, but it connects to OpenAI, Responses or vLLM APIs and is fully yours, in an isolated environment." },
  { title: "What's next?", body: "It's limited only by your imagination. This is a canvas where you build backends and stream UIs for your experiments and workflows, with AI assistants that understand the system from a first \`lookaround\` message." },
];
let at = 0;
const $ = (id) => document.getElementById(id);
function nodes(text) {
  const out = [], re = /\\[\\[([^\\]]+)\\]\\]|\`([^\`]+)\`/g;
  let last = 0, m;
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) out.push(document.createTextNode(text.slice(last, m.index)));
    const e = document.createElement(m[1] !== undefined ? "kbd" : "code");
    e.textContent = m[1] !== undefined ? m[1] : m[2];
    out.push(e);
    last = m.index + m[0].length;
  }
  if (last < text.length) out.push(document.createTextNode(text.slice(last)));
  return out;
}
function show(n) {
  if (TIPS.length === 0) return;
  at = ((n % TIPS.length) + TIPS.length) % TIPS.length;
  const t = TIPS[at];
  $("title").textContent = t.title;
  $("body").replaceChildren(...nodes(t.body));
  $("dots").replaceChildren(...TIPS.map((x, k) => {
    const d = document.createElement("button");
    d.className = "dot" + (k === at ? " on" : "");
    d.title = x.title;
    d.onclick = () => show(k);
    return d;
  }));
  $("count").textContent = (at + 1) + " of " + TIPS.length;
}
$("prev").onclick = () => show(at - 1);
$("next").onclick = () => show(at + 1);
$("card").addEventListener("keydown", (e) => {
  if (e.key === "ArrowLeft" || e.key === "ArrowRight") { e.preventDefault(); show(at + (e.key === "ArrowLeft" ? -1 : 1)); }
});
show(0);
</script>`;
