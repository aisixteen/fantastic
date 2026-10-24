import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { Host } from "../../lib.ts";
import type { Handler, Json } from "../../lib.ts";
import { panelRelay } from "../panel_relay.ts";
import { mountHostPanel } from "../agent_view/agent_view.ts";
import { ChatCards, cardRows, isCardRow } from "./chat_card.ts";
import { canvasNode, cellGrant, hostRecordFresh, viewOf } from "../canvas/host_views.ts";
import { hiddenOf, rectsOf, setHidden, themeOf } from "../canvas/layout.ts";
import { tint, tok } from "../skin/css.ts";

// ai_view — inline, MULTI-MODE AI view (chat mode = provider-agnostic chat UI). Ported from
// ai_chat_webapp/index.html, rendered INLINE into a canvas frame, fronting any
// backend that answers send/history/status (ollama_backend, vllm_backend,
// the browser-resident ai_backend). Preserves the full queue/inflight state
// machine, tool blocks, status phases, history+snapshot rebuild, lifecycle reset.
// THE ANSWER STREAMS AS PARTS (`part_start` / `part_delta` / `part_end` /
// `part_abort`, kind text | thinking | call): one DRAFT per `stream_id`, settled
// when the part ends — or NOT SENT on an abort or a malformed call. Stop / ESC
// is the backend's `stop` verb: the generation is cancelled, nothing is sent.
// ids → scoped classes so many chat frames coexist on one canvas page.

const CHAT_CSS = `
  .chat-view { display:flex; flex-direction:column; height:100%; min-height:0; overflow:hidden; font-family:${tok("font.family.sans")}; color:${tok("color.text.primary")}; background:transparent; box-sizing:border-box; }
  .chat-view .c-header { padding:8px 14px; font-size:12px; color:${tint("color.text.cool", 65)}; border-bottom:1px solid ${tint("color.ink", 6)}; display:flex; justify-content:space-between; }
  .chat-view .c-header b { color:${tint("color.text.cool", 95)}; }
  .chat-view .c-dialog { flex:1 1 0; min-height:0; overflow-y:auto; overscroll-behavior:contain; padding:12px; display:flex; flex-direction:column; gap:8px; }
  .chat-view .msg { padding:8px 12px; border-radius:${tok("radius.bubble")}; line-height:1.45; max-width:85%; word-wrap:break-word; white-space:pre-wrap; font-size:14px; box-shadow:${tok("shadow.bubble")}; }
  .chat-view .msg.user { align-self:flex-end; background:${tint("color.accent.deep", 32)}; color:${tok("color.text.bright")}; }
  .chat-view .msg.user.queued { opacity:0.55; font-style:italic; }
  .chat-view .msg.assistant { align-self:flex-start; background:${tok("color.surface.bubble")}; color:${tok("color.text.cool")}; }
  .chat-view .msg.notice { align-self:center; max-width:92%; text-align:center; font-size:11px; color:${tok("color.status.warn")}; background:${tint("color.status.warn", 12)}; box-shadow:inset 0 0 0 1px ${tint("color.status.warn", 18)}; }
  .chat-view .c-setup-help { color:${tok("color.text.primary")}; font-size:12.5px; background:${tint("color.accent.base", 12)}; box-shadow:inset 0 0 0 1px ${tint("color.accent.base", 35)}; }
  .chat-view .c-help-row { display:flex; gap:8px; justify-content:center; margin-top:8px; }
  .chat-view .c-help-row button { font:inherit; color:${tok("color.text.primary")}; background:${tok("color.surface.button")}; border:1px solid ${tok("color.line.button")}; border-radius:${tok("radius.card")}; padding:6px 12px; cursor:pointer; }
  .chat-view .c-help-row .c-help-settings { background:${tok("color.accent.base")}; border-color:${tok("color.accent.base")}; color:${tok("color.accent.on")}; font-weight:700; }
  .chat-view .c-inquiry { align-self:stretch; max-width:100%; background:${tint("color.status.warn", 10)}; box-shadow:inset 0 0 0 1px ${tint("color.status.warn", 45)}; color:${tok("color.status.warn-text")}; }
  .chat-view .c-inquiry .c-inq-head { font:600 10.5px ${tok("font.family.mono")}; letter-spacing:.1em; text-transform:uppercase; color:${tok("color.status.warn")}; margin-bottom:4px; }
  .chat-view .c-inquiry .c-inq-text { font-size:14px; white-space:pre-wrap; }
  .chat-view .c-inquiry .c-inq-row { display:flex; flex-wrap:wrap; gap:6px; align-items:center; margin-top:8px; }
  .chat-view .c-inquiry .c-inq-row button { font:inherit; font-size:13px; color:${tok("color.status.warn-text")}; background:${tok("color.status.warn-surface")}; border:1px solid ${tok("color.status.warn-line")}; border-radius:${tok("radius.card")}; padding:6px 12px; cursor:pointer; }
  .chat-view .c-inquiry .c-inq-row button.primary { background:${tok("color.status.warn")}; border-color:${tok("color.status.warn")}; color:${tok("color.status.warn-on")}; font-weight:700; }
  .chat-view .c-inquiry .c-inq-row label { font-size:13px; margin-right:6px; }
  .chat-view .c-inquiry .c-inq-input { flex:1; min-width:120px; font:inherit; font-size:13px; color:inherit; background:${tok("color.status.warn-field")}; border:1px solid ${tok("color.status.warn-line")}; border-radius:${tok("radius.card")}; padding:6px 10px; }
  .chat-view .c-inquiry .c-inq-status { font-size:12px; margin-top:6px; color:${tok("color.status.warn-muted")}; }
  .chat-view .c-inquiry.answered { opacity:.75; }
  .chat-view .c-stats { margin-left:auto; font:11px ${tok("font.family.mono")}; color:${tint("color.text.cool", 60)}; white-space:nowrap; }
  .chat-view .c-clear { font:11px ${tok("font.family.sans")}; color:${tint("color.text.cool", 80)}; background:${tint("color.ink", 5)}; border:1px solid ${tint("color.ink", 14)}; border-radius:${tok("radius.control")}; padding:2px 9px; cursor:pointer; margin-left:8px; }
  .chat-view .c-clear:hover { border-color:${tok("color.accent.base")}; color:${tok("color.text.bright")}; }
  .chat-view .c-malformed { align-self:stretch; max-width:100%; text-align:left; white-space:pre-wrap; font:11.5px/1.45 ${tok("font.family.mono")}; color:${tok("color.status.warn")}; }
  .chat-view .msg.notice.too-small { color:${tok("color.status.error")}; background:${tint("color.status.error", 12)}; box-shadow:inset 0 0 0 1px ${tint("color.status.error", 22)}; }
  .chat-view .c-card { flex:none; align-self:stretch; width:100%; box-sizing:border-box; border-radius:${tok("radius.card")}; background:${tok("color.surface.bubble")}; box-shadow:inset 0 0 0 1px ${tint("color.ink", 10)}; overflow:hidden; }
  .chat-view .c-card-head { display:flex; align-items:center; gap:4px; padding:2px 6px 2px 10px; font:11px ${tok("font.family.mono")}; color:${tint("color.text.cool", 60)}; position:relative; }
  .chat-view .c-card-title { flex:1; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  .chat-view .c-card-head button, .chat-view .c-card-show { font:inherit; color:inherit; background:transparent; border:0; cursor:pointer; padding:2px 6px; border-radius:${tok("radius.control")}; }
  .chat-view .c-card-head button:hover, .chat-view .c-card-show:hover { background:${tint("color.ink", 10)}; color:${tok("color.text.bright")}; }
  .chat-view .c-card-menu { display:none; position:absolute; right:6px; top:100%; z-index:2; flex-direction:column; background:${tok("color.surface.bubble")}; box-shadow:0 4px 14px ${tint("color.ink", 30)}; border-radius:${tok("radius.card")}; }
  .chat-view .c-card-menu[data-open="1"] { display:flex; }
  .chat-view .c-card-body { position:relative; width:100%; }
  .chat-view .c-card-body iframe { position:absolute; inset:0; display:block; width:100%; height:100%; border:0; background:transparent; }
  .chat-view .c-card-deleted { width:100%; font:12px ${tok("font.family.mono")}; color:${tok("color.status.warn")}; background:${tint("color.status.warn", 10)}; border:0; padding:10px; cursor:pointer; }
  .chat-view .c-card-hiddenbar { display:flex; align-items:center; gap:6px; padding:4px 10px; font:11px ${tok("font.family.mono")}; color:${tint("color.text.cool", 55)}; }
  .chat-view .tool-block.recall .tool-verb { color:${tok("color.status.info")}; }
  .chat-view .tool-block { margin:6px 0; padding:6px 8px; border-radius:${tok("radius.card")}; background:${tok("color.surface.code")}; box-shadow:inset 0 0 0 1px ${tint("color.ink", 6)}; font-family:${tok("font.family.mono")}; font-size:12px; color:${tok("color.text.code")}; }
  .chat-view .tool-block .tool-head { display:flex; align-items:baseline; gap:6px; }
  .chat-view .tool-block .tool-verb { color:${tok("color.status.warn")}; }
  .chat-view .tool-block .tool-target { color:${tok("color.status.info-muted")}; }
  .chat-view .tool-block .tool-state { margin-left:auto; color:${tok("color.text.dim")}; font-size:11px; }
  .chat-view .tool-block.pending .tool-state::after { content:'…'; animation:cpulse ${tok("duration.pulse")} ${tok("easing.in-out")} infinite; display:inline-block; }
  .chat-view .tool-block .tool-detail summary { cursor:pointer; color:${tok("color.text.dim")}; font-size:11px; }
  .chat-view .tool-block pre { margin:4px 0; padding:6px; background:${tok("color.surface.well")}; border-radius:${tok("radius.chip")}; color:${tok("color.text.code")}; white-space:pre-wrap; word-break:break-all; max-height:160px; overflow-y:auto; }
  .chat-view .c-header, .chat-view .c-footer, .chat-view .c-row { flex:none; }
  .chat-view .c-queued { flex:none; max-height:30%; overflow-y:auto; display:flex; flex-direction:column; gap:4px; padding:0 12px; }
  .chat-view .c-queued:empty { display:none; }
  .chat-view .c-queued .msg.user { max-width:100%; opacity:0.55; font-style:italic; }
  .chat-view .c-footer { display:flex; align-items:center; gap:10px; padding:6px 14px; border-top:1px solid ${tint("color.ink", 6)}; font-size:11px; color:${tint("color.text.cool", 55)}; min-height:28px; }
  .chat-view .phase-pill { display:inline-block; padding:2px 8px; border-radius:${tok("radius.pill")}; background:${tint("color.ink", 8)}; color:${tint("color.text.cool", 75)}; font-weight:500; }
  .chat-view .phase-pill[data-phase="thinking"],.chat-view .phase-pill[data-phase="tool_calling"] { background:${tint("color.accent.deep", 45)}; color:${tok("color.text.bright")}; animation:cpulse ${tok("duration.pulse")} ${tok("easing.in-out")} infinite; }
  .chat-view .phase-pill[data-phase="streaming"] { background:${tint("color.status.ok-deep", 40)}; color:${tok("color.text.bright")}; }
  .chat-view .phase-pill[data-phase="queued"] { background:${tint("color.status.warn", 20)}; color:${tok("color.status.warn")}; }
  .chat-view .others-hint { color:${tok("color.text.dim")}; font-size:10px; }
  @keyframes cpulse { 0%,100%{opacity:1;} 50%{opacity:0.45;} }
  .chat-view .c-row { display:flex; padding:10px; gap:8px; border-top:1px solid ${tint("color.ink", 6)}; }
  .chat-view .c-input { flex:1; background:${tint("color.ink", 6)}; border:0; box-shadow:inset 0 0 0 1px ${tint("color.ink", 10)}; border-radius:${tok("radius.panel")}; padding:8px 12px; color:${tok("color.text.primary")}; font-size:14px; outline:none; }
  .chat-view .c-input:focus { box-shadow:${tok("shadow.focus")}; }
  .chat-view .c-send { background:${tint("color.accent.deep", 45)}; border:0; box-shadow:${tok("shadow.button")}; border-radius:${tok("radius.panel")}; color:${tok("color.text.bright")}; padding:8px 14px; cursor:pointer; }
  .chat-view .c-send:hover { background:${tint("color.accent.deep-hover", 55)}; }
  .chat-view .c-send.stop { background:${tint("color.status.error-deep", 55)}; }
  .chat-view .msg .spot { display:block; font-size:10px; opacity:0.6; margin-bottom:2px; font-family:${tok("font.family.mono")}; }
  .chat-view .c-badge { font-size:11px; margin-left:8px; padding:1px 8px; border-radius:${tok("radius.pill")}; background:${tint("color.ink", 8)}; color:${tint("color.text.cool", 75)}; }
  .chat-view .msg.thinking { color: ${tok("color.accent.base")}; font-style: italic; animation: c-think ${tok("duration.think")} ${tok("easing.in-out")} infinite; }
  .chat-view .c-think { padding:4px 12px; background:${tint("color.accent.base", 6)}; box-shadow:inset 0 0 0 1px ${tint("color.accent.base", 22)}; color:${tok("color.accent.soft")}; font-style:normal; }
  .chat-view .c-think.thinking { animation: none; }
  .chat-view .c-think.thinking summary { animation: c-think ${tok("duration.think")} ${tok("easing.in-out")} infinite; }
  .chat-view .c-think summary { cursor:pointer; font:italic 12px ${tok("font.family.sans")}; color:${tok("color.accent.base")}; list-style:revert; user-select:none; }
  .chat-view .c-think .c-think-body { white-space:pre-wrap; font-size:12.5px; line-height:1.45; color:${tint("color.accent.text", 78)}; margin-top:6px; max-height:320px; overflow:auto; }
  @keyframes c-think { 0%, 100% { opacity: 0.35; } 50% { opacity: 1; } }
  .chat-view .msg.not-sent { opacity: 0.45; text-decoration: line-through; }
  .chat-view .msg.not-sent::after { content: ' · not sent — ' attr(data-why); font-size: 10px; text-decoration: none; }
  .chat-view .msg.monitor { align-self:flex-start; max-width:92%; background:${tint("color.status.monitor", 10)}; box-shadow:inset 0 0 0 1px ${tint("color.status.monitor", 25)}; color:${tok("color.status.monitor-text")}; font:11.5px/1.45 ${tok("font.family.mono")}; white-space:pre-wrap; }
  .chat-view .msg.monitor .mon-tag { display:block; margin-bottom:3px; font-size:10.5px; color:${tok("color.status.ok")}; letter-spacing:.02em; }
  .chat-view .msg.monitor .mon-tag .ended { color:${tok("color.status.warn")}; }
  .chat-view .msg.tool { align-self: flex-start; font: 11px ${tok("font.family.mono")}; color: ${tint("color.text.cool", 45)}; background: transparent; padding: 0 4px; }
  .chat-view .c-badge.kept { background:${tint("color.status.ok", 18)}; color:${tok("color.status.ok")}; cursor:pointer; }
  .chat-view .c-keep { margin-left:6px; font-size:11px; padding:1px 10px; border-radius:${tok("radius.pill")}; border:0; cursor:pointer; background:${tint("color.accent.base", 28)}; color:${tok("color.text.bright")}; }
  .chat-view .c-main.is-main { background:${tint("color.status.gold", 22)}; color:${tok("color.status.gold")}; }
  .chat-view .c-rename { font-size:11px; margin-left:8px; padding:1px 6px; border-radius:${tok("radius.control")}; border:0; background:${tint("color.ink", 12)}; color:${tok("color.text.bright")}; width:140px; }
`;

/** An AI cell's turn carries WHERE it was typed, as the LAST line of its text
 *  (so it is in the conversation's own rows, and a history digest — the first
 *  line — still shows what was said). */
const SPOT = /\n?\[at (-?\d+),(-?\d+)\]$/;
/** The `[from <sender> …]` line ai_core opens a non-operator's message with —
 *  the cell's own sends come through the leg, so the stored turn carries it. */
const FROM = /^\[from [^\]]*\]\s*/;

/** The model's calls are the harness's business (tool blocks show them); the
 *  bubble shows its words. Complete `<call …>…</call>` blocks are hidden. */
const CALLS = /<call\b[^>]*>[\s\S]*?<\/call>/g;
/** A call cut off mid-way (a history digest's head, a stream in flight). */
const OPEN_CALL = /<call\b[\s\S]*$/;
/** A model's reasoning, when it arrives inline (`<think>…</think>`, or cut off). */
const THINK = /<think>[\s\S]*?(?:<\/think>|$)/g;
const words = (t: string): string => t.replace(THINK, "").replace(CALLS, "").replace(OPEN_CALL, "").trim();
/** The first call of a calls-only turn: its name, target and verb. */
const CALL_HEAD = /<call name="([^"]+)">(?:[\s\S]*?<target_id>([^<]*)<\/target_id>)?(?:[\s\S]*?<type>([^<]*)<\/type>)?/;
/** AN EXEC_BRIDGE MONITOR'S DELIVERY (the lib, 2026-10-09): a `send {text}` from
 *  the runner, stored as `[from <runner>]` then
 *  `[monitor 787 · job 514 · from your call 783 · "w5" · job ended]` then the
 *  matched lines. A NOTIFICATION, never a question waiting on the human. */
const MONITOR_HEAD = /^\[monitor (\d+) · job (\d+) · from (.+?) call (\S+?)(?: · "([^"]*)")?( · job ended)?\]\s*/;

export interface MonitorDelivery {
  id: string;
  job: string;
  /** whose call started the job: `your` / `<caller>'s` / `the operator's` */
  who: string;
  label: string | null;
  /** the job ended: the monitor's last delivery */
  ended: boolean;
  /** the matched lines, the headers stripped */
  lines: string;
}

/** A stored turn that is a monitor delivery → its parts; else null. */
export function monitorOf(text: string): MonitorDelivery | null {
  const from = /^\[from [^\]]*\]\s*/.exec(text);
  if (from === null) return null;
  const rest = text.slice(from[0].length);
  const m = MONITOR_HEAD.exec(rest);
  if (m === null) return null;
  return { id: m[1]!, job: m[2]!, who: m[3]!, label: m[5] ?? null, ended: m[6] !== undefined, lines: rest.slice(m[0].length).replace(/\s+$/, "") };
}

/** A monitor delivery's tag: `monitor 787 · job 514 "w5" · ended`. */
export function monitorTag(d: MonitorDelivery): string {
  return `monitor ${d.id} · job ${d.job}${d.label !== null ? ` "${d.label}"` : ""}`;
}

/** The CALL CHECKER's report: the person sees why nothing ran. */
export function isReport(m: Record<string, unknown>): boolean {
  return typeof m["head"] === "string" && m["head"].includes("[malformed call — nothing ran]");
}

/** The newest `keep` history rows, oldest first, over every page (history answers
 *  oldest-first, one page at a time: `next` is the cursor for the following page).
 *  `bytes` sums every row's bytes; `lastId` is the highest row id seen. */
export async function historyTail(
  call: (p: Record<string, unknown>) => Promise<Record<string, unknown> | null>,
  keep = 300,
): Promise<{ rows: Record<string, unknown>[]; bytes: number; lastId: number }> {
  let window: Record<string, unknown>[] = [];
  let bytes = 0;
  let lastId = 0;
  let args: Record<string, unknown> = { type: "history" };
  for (let page = 0; page < 1000; page++) {
    const r = await call(args);
    if (r === null) break;
    const rows = Array.isArray(r["rows"]) ? (r["rows"] as Record<string, unknown>[]) : [];
    for (const m of rows) {
      if (typeof m["bytes"] === "number") bytes += m["bytes"] as number;
      if (typeof m["id"] === "number") lastId = Math.max(lastId, m["id"] as number);
    }
    window = window.concat(rows);
    if (window.length > keep) window = window.slice(window.length - keep);
    if (typeof r["next"] !== "number") break;
    args = { type: "history", after: r["next"] };
  }
  return { rows: window, bytes, lastId };
}

/** A run of CONSECUTIVE report rows with the same head becomes the first of them
 *  with `repeat: n` (n >= 2); every other row is unchanged. */
export function collapseReports(
  rows: Record<string, unknown>[],
  isRep: (m: Record<string, unknown>) => boolean,
): Record<string, unknown>[] {
  const out: Record<string, unknown>[] = [];
  for (const m of rows) {
    const last = out[out.length - 1];
    if (isRep(m) && last !== undefined && isRep(last) && last["head"] === m["head"]) {
      last["repeat"] = typeof last["repeat"] === "number" ? (last["repeat"] as number) + 1 : 2;
      continue;
    }
    out.push({ ...m });
  }
  return out;
}

/** ai_core's header on a reply that comes back as the next turn. */
const REPLY_HEAD = /^\[reply to \d+ · ([^\]·]+?)(?: · [^\]]*)?\]/;

/** THE SETUP, told once — on a cell's FIRST turn, inside the turn itself
 *  (after the typed words, before `[at X,Y]`), never as `system_prompt`: that
 *  field REPLACES ai_core's default system block (the call grammar, the verbs,
 *  the agent's identity), which a cell must keep. It explains the two levels
 *  and the two kernels and says how to learn them — by reflecting each — but
 *  names NO address beyond the host's reserved `kernel`: finding the page is
 *  the AI's own work. */
export const CANVAS_SETUP =
  
  "[canvas setup] You are an AI agent on the HOST kernel: your record, memory and calls live there. A human sees you as a CELL on a CANVAS — a web page that is a SECOND kernel, connected as a peer: its agents are `<page guid>|<agent id>` (its kernel `<page guid>|kernel`). Reflect the host first (it lists the peers), then the page. To BUILD on the canvas, pull `docs of=<page guid>|view_root` first: the view places cells, and its doc carries the panel API. A TOOL with its own panel is ONE host agent: create it with `create` on `{{node}}` — THIS canvas's host node (`create {record: {handler_module, …, view: {kind: html, bytes, calls: [the verbs its panel calls]}}}`) and show it with `place {backend_id, x, y}` on the view — the panel's grants follow from `calls`. A PYTHON agent is an instance of a BUNDLE (code in `.fantastic/bundles/<name>/`): editing the bundle's files changes EVERY instance at once; to change one alone, copy the folder to a new name first (`fantastic readme bundles`). A LIVE panel — a host agent that pushes updates into its own cell (streaming to its panel) — is the python_agent STARTER: `spawn {bundle: python_agent}` on `{{node}}` makes a new bundle and one instance; its `agent.py` sends `fa.emit({type, …})`, its `panel.html` redraws on `fantastic.watch(<agent id>, ev => …)`, and its `bundle.yaml` record carries the view (`view: {kind: html, bytes_file: panel.html, calls: [<verbs>, watch]}`); show it with `place {backend_id}`. To show a host agent's panel INSIDE THIS CHAT instead (a chat card, 100% of the column wide, at most 1200 px tall), call `embed {id}` on your own agent — its docs say how to design the panel. Every user turn ends with [at X,Y], where it was typed (x grows right, y grows down).";
/** The setup as the TRANSCRIPT carries it — hidden from the bubble. */
const SETUP_BLOCK = /\n\[canvas setup\][\s\S]*$/;

/** The chat cell's own verbs: only the relay of its cards' panels (a card's calls go out AS
 *  the cell, judged by its `can_call`; the cell holds no content). */
export const aiViewAgent: Handler = (_id, payload, kernel): Json | Promise<Json> => panelRelay(kernel, payload) ?? null;

function ensureStyle(): void {
  if (document.getElementById("fantastic-chat-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-chat-style";
  s.textContent = CHAT_CSS;
  document.head.appendChild(s);
}

type Dict = Record<string, unknown>;
const s = (d: Dict, k: string): string => (typeof d[k] === "string" ? (d[k] as string) : "");
const obj = (d: Dict, k: string): Dict => (d[k] !== null && typeof d[k] === "object" ? (d[k] as Dict) : {});

interface ToolHandle {
  el: HTMLElement;
  headEl: HTMLElement;
  replyEl: HTMLElement;
}
interface Inflight {
  send_id: string;
  userBubble: HTMLElement;
  /** where this turn's tool blocks land (the first text draft, or the dialog) */
  toolBlocks: Map<string, ToolHandle>;
}
/** One streamed part, drawn while the model writes it. */
interface Draft {
  el: HTMLElement;
  kind: string;
  text: string;
}

// Non-chat modes are SCAFFOLDED seams: the shell + backend binding live here; the
// mode-specific UX lands later. `agent-prompt` → edit the worker's stored system
// prompt; `dynamic-buttons` → a grid of declared actions that `send` to the backend.
function mountStub(
  mount: HTMLElement,
  mode: string,
  selfId: string,
  backend: string,
): ViewHandle {
  ensureStyle();
  const root = document.createElement("div");
  root.className = "chat-view";
  const header = document.createElement("div");
  header.className = "c-header";
  const left = document.createElement("span");
  const b = document.createElement("b");
  b.textContent = selfId;
  left.appendChild(b);
  const right = document.createElement("span");
  right.textContent = `mode: ${mode}`;
  header.append(left, right);
  const dialog = document.createElement("div");
  dialog.className = "c-dialog";
  const msg = document.createElement("div");
  msg.className = "msg assistant";
  msg.textContent =
    `ai_view mode "${mode}" is scaffolded (backend ${backend}). ` +
    `chat is the wired mode; agent-prompt / dynamic-buttons land here.`;
  dialog.appendChild(msg);
  root.append(header, dialog);
  mount.appendChild(root);
  return {
    unmount(): void {
      root.remove();
    },
  };
}

export const aiView: ViewBundle = {
  // The frontend member is an `ai_view.ts` agent (a `backend_id` peer ref to any
  // host AI/chat backend — vllm / responses / ollama). A `mode` field on the
  // record selects the renderer: `chat` is fully wired; `agent-prompt` and
  // `dynamic-buttons` are scaffolded seams (mountStub) for later.
  handles: ["ai_view.ts"],
  // Fronts a backend it never spawns; cancels a turn through the host root's
  // task table.
  grants: (backend) => ({ [backend]: ["watch", "reflect", "history", "rows", "status", "send", "stop", "context_clear", "unembed"], kernel: ["ps", "cancel"], [canvasNode()]: ["rename"] }),

  async mount(ctx: ViewContext): Promise<ViewHandle> {
    const { kernel, mount, selfId, backend, record } = ctx;
    ensureStyle();
    const mode =
      record && typeof record.mode === "string" && record.mode ? record.mode : "chat";
    if (mode !== "chat") return mountStub(mount, mode, selfId, backend);
    const host = new Host(kernel, selfId);

    const root = document.createElement("div");
    root.className = "chat-view";
    root.innerHTML = `
      <div class="c-header"><span><b class="c-agent"></b></span><span class="c-model"></span></div>
      <div class="c-dialog"></div>
      <div class="c-queued"></div>
      <div class="c-footer">
        <span class="phase-pill" data-phase="idle">idle</span>
        <span class="elapsed"></span><span class="hint"></span><span class="others-hint"></span>
        <span class="c-stats" title="the last generation's speed · the context used of the window — ~ marks an estimate (≈4 characters a token; the conversation only) until the backend reports usage"></span>
        <button class="c-clear" title="start a new thread: the AI's view begins here (nothing is deleted — recall pages back)">clear</button>
      </div>
      <div class="c-row"><input class="c-input" placeholder="message…"><button class="c-send">send</button></div>
    `;
    mount.appendChild(root);
    const $ = <T extends HTMLElement>(sel: string): T => root.querySelector(sel) as T;
    const dialogEl = $<HTMLElement>(".c-dialog");
    const queuedEl = $<HTMLElement>(".c-queued");
    const inputEl = $<HTMLInputElement>(".c-input");
    const sendBtn = $<HTMLButtonElement>(".c-send");
    const phasePill = $<HTMLElement>(".phase-pill");
    const elapsedEl = $<HTMLElement>(".elapsed");
    const hintEl = $<HTMLElement>(".hint");
    const othersHintEl = $<HTMLElement>(".others-hint");
    // SPEED AND CONTEXT (owner 2026-10-11): the last generation's tok/s and the
    // context it used of the window — from the `done` status's usage, when the
    // backend reports it; the window alone until then
    const statsEl = $<HTMLElement>(".c-stats");
    let windowTokens: number | null = null;
    let genChars = 0; // characters streamed to this cell in the current generation
    let genStart = 0; // when its first one arrived
    let lastRate: number | null = null; // the last generation's estimated tok/s
    let lastPrompt: string | null = null; // the last reported prompt size, as shown
    let lastSpeed: string | null = null; // the last generation's speed, as shown
    let convBytes = 0; // the conversation's size (history bytes) since the last clear
    const liveRate = (): number => Math.round(genChars / 4 / Math.max(0.25, (Date.now() - genStart) / 1000));
    const kTok = (n: number): string => (n >= 1000 ? `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}k` : String(Math.round(n)));
    const showStats = (detail: Dict): void => {
      const num = (k: string): number | null => (typeof detail[k] === "number" && Number.isFinite(detail[k] as number) ? (detail[k] as number) : null);
      const cw = num("context_window") ?? windowTokens;
      if (cw !== null) windowTokens = cw;
      const out = num("completion_tokens");
      const secs = num("gen_seconds");
      const prompt = num("prompt_tokens");
      const bits: string[] = [];
      // the backend's usage (`*_estimated`: its own estimate, not the provider's)
      const est = (k: string): string => (detail[`${k}_estimated`] === true ? "~" : "");
      if (out !== null && secs !== null && secs > 0) lastSpeed = `${est("completion_tokens")}${Math.round(out / secs)} tok/s`;
      else if (lastSpeed === null && lastRate !== null) lastSpeed = `~${lastRate} tok/s`;
      if (lastSpeed !== null) bits.push(lastSpeed);
      if (prompt !== null) lastPrompt = `${est("prompt_tokens")}${kTok(prompt)}`;
      if (cw !== null) bits.push(`${lastPrompt ?? (convBytes > 0 ? `~${kTok(convBytes / 4)}` : "—")} / ${kTok(cw)} ctx`);
      statsEl.textContent = bits.join(" · ");
    };
    // CLEAR: a new thread on the same store (`context_clear`) — nothing deleted
    $<HTMLButtonElement>(".c-clear").addEventListener("click", () => {
      void host.call(backend, { type: "context_clear", why: "cleared from the chat" } as never).then((r) => {
        const o = r as Dict | null;
        if (o && typeof o["error"] === "string") addMsg("notice", `✕ ${o["error"]}`, dialogEl).classList.add("too-small");
        else {
          addMsg("notice", "— cleared: a new thread starts here (the earlier one is kept — recall pages back) —", dialogEl);
          convBytes = 0;
          lastPrompt = null;
          showStats({});
        }
      });
    });
    // STICKY BOTTOM: every change to the dialog (a message, a streamed part, a
    // tool block) scrolls it down while the reader is at the bottom; scrolled up
    // to read, it stays put until they come back down (or send).
    let pinned = true;
    const toBottom = (): void => {
      dialogEl.scrollTop = dialogEl.scrollHeight;
    };
    dialogEl.addEventListener("scroll", () => {
      pinned = dialogEl.scrollHeight - dialogEl.scrollTop - dialogEl.clientHeight < 48;
    });
    const stick = new MutationObserver(() => {
      if (pinned) toBottom();
    });
    stick.observe(dialogEl, { childList: true, subtree: true, characterData: true });

    // client_id: stable per backend, localStorage so refresh keeps the thread
    const CLIENT_KEY = `ai_chat_client_id:${backend}`;
    // storage may be blocked (a private window): then the id lives for this mount
    let clientId: string | null = null;
    try {
      clientId = localStorage.getItem(CLIENT_KEY);
    } catch {
      /* blocked storage */
    }
    if (clientId === null) {
      clientId = "web_" + Math.random().toString(36).slice(2, 10);
      try {
        localStorage.setItem(CLIENT_KEY, clientId);
      } catch {
        /* blocked storage */
      }
    }
    // CHAT CARDS (#339): host agents' panels shown inline, where `embed` rows sit in the
    // conversation. The cards' calls go out AS this cell, so each card's grant
    // (`cellGrant`: the record's `view.calls`) is added to the cell's `can_call` as it mounts.
    const chatView = (): string => kernel.get(selfId)?.parentId ?? "";
    const cards = new ChatCards({
      chat: selfId,
      recordOf: (a) => hostRecordFresh(a),
      mountPanel: (el, agent, rec, sizeKey) => {
        const have = kernel.get(selfId)?.meta["can_call"];
        kernel.updateMeta(selfId, { can_call: { ...(have !== null && typeof have === "object" ? (have as Dict) : {}), ...cellGrant(agent, viewOf(rec)) } } as never);
        return mountHostPanel({ ...ctx, mount: el }, agent, { sizeKey, cellInbox: false });
      },
      hidden: {
        has: (k) => hiddenOf(kernel, chatView()).has(k),
        set: (k, on) => setHidden(kernel, chatView(), k, on),
      },
      unembed: (a) => host.call(backend, { type: "unembed", id: a } as never),
      deleteAgent: (a) => ctx.actions?.deleteAgent?.(a),
      expand: (a) => ctx.actions?.viewFull?.(a),
      confirm: (m) => (typeof confirm === "function" ? confirm(m) : false),
      toast: (m) => (ctx.actions?.toast !== undefined ? ctx.actions.toast(m) : console.warn("[chat]", m)),
      notify: (text) => {
        void host.call(backend, { type: "send", text, client_id: clientId } as never);
      },
    });
    // an embed row is a card, an unembed row takes it out: applied in the order they came
    const applyCardRow = (m: Dict): void => {
      const agent = s(m, "head");
      if (agent === "") return;
      if (s(m, "kind") === "unembed") cards.remove(agent);
      else if (!cards.has(agent)) dialogEl.appendChild(cards.draw(agent, true));
    };
    // a deleted agent: its card turns "deleted" at once (the host's lifecycle frame)
    const offState = host.subscribeState((frame) => cards.onState(frame));
    const tpl = record && typeof record["saved"] === "string" ? (record["saved"] as string) : "";
    $<HTMLElement>(".c-agent").textContent = tpl ? `✦ ${tpl} · ${backend}` : `${selfId} (client ${clientId})`;
    // THE DIALOG'S NAME, in the header (owner 2026-10-12: no ephemeral chats —
    // every dialog is kept from its start, as in the terminal): click to rename
    if (tpl) {
      const badge = document.createElement("span");
      badge.className = "c-badge kept";
      const show = (name: string): void => {
        badge.textContent = name;
        badge.title = "click to rename this dialog";
      };
      show(record && typeof record["dialog_name"] === "string" ? (record["dialog_name"] as string) : backend);
      badge.onclick = (): void => {
        const input = document.createElement("input");
        input.className = "c-rename";
        input.value = badge.textContent ?? "";
        badge.replaceWith(input);
        input.focus();
        input.select();
        const done = async (save: boolean): Promise<void> => {
          if (save && input.value.trim()) {
            const r = (await host.callHost(canvasNode(), { type: "rename", id: backend, name: input.value.trim() } as never)) as Dict;
            if (typeof r["error"] !== "string") {
              kernel.updateMeta(selfId, { dialog_name: r["name"], kept: true } as never);
              show(String(r["name"]));
            } else addMsg("notice", `✕ ${r["error"]}`, dialogEl).classList.add("too-small");
          }
          input.replaceWith(badge);
        };
        input.onkeydown = (e: KeyboardEvent): void => {
          if (e.key === "Enter") void done(true);
          if (e.key === "Escape") void done(false);
        };
        input.onblur = (): void => void done(true);
      };
      $<HTMLElement>(".c-agent").after(badge);
    }
    const upRefl = (await host.call(backend, { type: "reflect" })) as Dict;
    $<HTMLElement>(".c-model").textContent = s(upRefl, "model");
    showStats(typeof upRefl["context_window"] === "number" ? { context_window: upRefl["context_window"] } : {});

    const addMsg = (role: string, text: string, parent: HTMLElement): HTMLElement => {
      const div = document.createElement("div");
      // A TOOL ROUND-TRIP is the harness's, not the dialog's: a turn that is only
      // calls, and the reply that comes back as the next (user-role) turn, show
      // as one quiet line each — `→ target verb` / `↳ target verb`.
      const call = role === "assistant" && words(text) === "" ? CALL_HEAD.exec(text) : null;
      // THE CALL CHECKER'S REPORT (ai_core): the model wrote a call that did not
      // parse — nothing ran, and it is told how to write it. A notice, not a
      // bubble of the human's
      const bad = role === "user" ? text.indexOf("[malformed call — nothing ran]") : -1;
      if (bad >= 0) {
        div.className = "msg notice c-malformed";
        div.textContent = text.slice(bad);
        parent.appendChild(div);
        if (parent === dialogEl) dialogEl.scrollTop = dialogEl.scrollHeight;
        return div;
      }
      const reply = role === "user" ? REPLY_HEAD.exec(text) : null;
      if (call !== null || reply !== null) {
        div.className = "msg tool";
        div.textContent = call !== null ? `→ ${call[2] ?? call[1]} ${call[3] ?? ""}`.trim() : `↳ ${reply![1]}`;
        parent.appendChild(div);
        if (parent === dialogEl) dialogEl.scrollTop = dialogEl.scrollHeight;
        return div;
      }
      // a MONITOR's delivery: a notification from the runner — its tag and the
      // matched lines, never a bubble of the human's
      const mon = role === "user" ? monitorOf(text) : null;
      if (mon !== null) {
        div.className = "msg monitor";
        const tag = document.createElement("span");
        tag.className = "mon-tag";
        tag.textContent = monitorTag(mon);
        if (mon.ended) {
          const e = document.createElement("span");
          e.className = "ended";
          e.textContent = " · ended";
          tag.append(e);
        }
        div.append(tag, mon.lines);
        parent.appendChild(div);
        if (parent === dialogEl) dialogEl.scrollTop = dialogEl.scrollHeight;
        return div;
      }
      div.className = "msg " + role;
      const spot = role === "user" ? SPOT.exec(text) : null;
      if (spot !== null) {
        const tag = document.createElement("span");
        tag.className = "spot";
        tag.textContent = `you · at ${spot[1]},${spot[2]}`;
        div.append(tag, text.slice(0, spot.index).replace(SETUP_BLOCK, "").replace(FROM, ""));
      } else {
        div.textContent = role === "assistant" ? words(text) : text;
      }
      parent.appendChild(div);
      if (parent === dialogEl) dialogEl.scrollTop = dialogEl.scrollHeight;
      return div;
    };

    const queuedBubbles = new Map<string, HTMLElement>();
    const queuedOrder: string[] = [];
    const localPending: string[] = [];
    let inflight: Inflight | null = null;

    const refreshBusy = (): void => {
      // the queue map mirrors the QUEUED bubbles only: an entry whose bubble
      // already moved into the dialog (promoted) is not a pending turn — left
      // behind, it kept the button on "stop" after the answer, so the next Send
      // interrupted instead (measured, E45)
      for (const [k, b] of [...queuedBubbles]) {
        if (b.parentElement !== queuedEl) {
          queuedBubbles.delete(k);
          const i = queuedOrder.indexOf(k);
          if (i >= 0) queuedOrder.splice(i, 1);
          const j = localPending.indexOf(k);
          if (j >= 0) localPending.splice(j, 1);
        }
      }
      const on = inflight !== null || queuedBubbles.size > 0;
      sendBtn.textContent = on ? "stop" : "send";
      sendBtn.classList.toggle("stop", on);
    };

    let elapsedTimer: ReturnType<typeof setInterval> | null = null;
    let elapsedStart = 0;
    const setPhase = (phase: string, hint = "", reason = ""): void => {
      phasePill.dataset["phase"] = phase;
      // why the last turn ended (`done`'s reason): `awaiting` = calls still in
      // flight, the chain goes on; anything else = the chain is over
      phasePill.dataset["reason"] = reason;
      phasePill.textContent = phase;
      hintEl.textContent = hint;
      const live = phase === "thinking" || phase === "streaming" || phase === "tool_calling" || phase === "queued";
      if (live) {
        if (elapsedTimer === null) {
          elapsedStart = Date.now();
          genChars = 0;
          genStart = 0;
          // SPEED, LIVE (owner 2026-10-12: tok/s, not seconds): ~4 characters a
          // token over what streamed so far — the backend's own count replaces it
          // when its `done` reports usage; seconds only until the first token
          elapsedTimer = setInterval(() => {
            elapsedEl.textContent = genChars > 0 ? `~${liveRate()} tok/s` : `${((Date.now() - elapsedStart) / 1000).toFixed(1)}s`;
          }, 250);
        }
      } else if (elapsedTimer !== null) {
        clearInterval(elapsedTimer);
        elapsedTimer = null;
        elapsedEl.textContent = "";
        if (genChars > 0) {
          lastRate = liveRate();
          lastSpeed = null; // this generation's: its usage, or the estimate, on the next draw
        }
      }
    };
    const setOthersHint = (n: number): void => {
      othersHintEl.textContent = n > 0 ? `+${n} from other clients` : "";
    };
    setPhase("idle");

    // set once the card reader below exists (a tool reply drawn at mount comes before it)
    let cardsSoonImpl: (() => void) | null = null;
    const cardsSoon = (): void => cardsSoonImpl?.();

    const makeToolBlock = (tool: Dict): ToolHandle => {
      const el = document.createElement("div");
      el.className = "tool-block pending";
      if (s(tool, "verb") === "recall") el.classList.add("recall");
      const head = document.createElement("div");
      head.className = "tool-head";
      head.innerHTML = `<span class="tool-verb"></span>(<span class="tool-target"></span>)<span class="tool-state"></span>`;
      (head.querySelector(".tool-verb") as HTMLElement).textContent = s(tool, "verb");
      (head.querySelector(".tool-target") as HTMLElement).textContent = s(tool, "target");
      el.appendChild(head);
      const det = document.createElement("details");
      det.className = "tool-detail";
      const sum = document.createElement("summary");
      sum.textContent = "args";
      det.appendChild(sum);
      const argsPre = document.createElement("pre");
      argsPre.textContent = JSON.stringify(obj(tool, "args"), null, 2);
      det.appendChild(argsPre);
      const replyPre = document.createElement("pre");
      det.appendChild(replyPre);
      el.appendChild(det);
      return { el, headEl: head, replyEl: replyPre };
    };
    const fillToolReply = (h: ToolHandle, preview: string): void => {
      h.replyEl.textContent = preview;
      h.el.classList.remove("pending");
      (h.headEl.querySelector(".tool-state") as HTMLElement).textContent = "✓";
    };

    const bindNextLocalToSendId = (sendId: string): void => {
      if (!sendId || queuedBubbles.has(sendId)) return;
      const localKey = localPending.shift();
      if (localKey === undefined) return;
      const bubble = queuedBubbles.get(localKey);
      if (!bubble) return;
      queuedBubbles.delete(localKey);
      queuedBubbles.set(sendId, bubble);
      const i = queuedOrder.indexOf(localKey);
      if (i >= 0) queuedOrder[i] = sendId;
    };

    const promoteToInflight = (sendId: string, fallback: string): void => {
      let bubble = queuedBubbles.get(sendId);
      if (!bubble && localPending.length > 0) {
        bindNextLocalToSendId(sendId);
        bubble = queuedBubbles.get(sendId);
      }
      if (!bubble) {
        // a turn with no user text of its own (a reply coming back to the model)
        // gets no bubble — the tool line already stands for it
        bubble = fallback ? addMsg("user", fallback, dialogEl) : document.createElement("div");
      } else {
        bubble.classList.remove("queued");
        bubble.style.maxWidth = "";
        bubble.style.opacity = "";
        dialogEl.appendChild(bubble);
        queuedBubbles.delete(sendId);
        const i = queuedOrder.indexOf(sendId);
        if (i >= 0) queuedOrder.splice(i, 1);
      }
      inflight = { send_id: sendId, userBubble: bubble, toolBlocks: new Map() };
      refreshBusy();
    };

    const handleToolCall = (tool: Dict): void => {
      if (inflight === null) return;
      const callId = s(tool, "call_id");
      let handle = inflight.toolBlocks.get(callId);
      if (!handle) {
        handle = makeToolBlock(tool);
        inflight.toolBlocks.set(callId, handle);
        dialogEl.appendChild(handle.el);
        dialogEl.scrollTop = dialogEl.scrollHeight;
      }
      if ("reply_preview" in tool) {
        fillToolReply(handle, s(tool, "reply_preview"));
        // the AI's own `embed` / `unembed` just answered: its card row is in the history now —
        // draw it at once, not when the turn ends
        if (s(tool, "target") === backend && (s(tool, "verb") === "embed" || s(tool, "verb") === "unembed")) cardsSoon();
      }
    };

    // lifecycle: drop stale inflight on disconnect (no `done` will arrive)
    const offLifecycle = host.onLifecycle(backend, (state) => {
      if (state === "disconnected") {
        inflight = null;
        for (const id of [...drafts.keys()]) settle(id, "disconnected");
        for (const [, b] of queuedBubbles) b.remove();
        queuedBubbles.clear();
        queuedOrder.length = 0;
        localPending.length = 0;
        refreshBusy();
        setPhase("idle", "disconnected — waiting to reconnect");
      } else {
        setPhase("idle", "");
      }
    });

    const mine = (p: Dict): boolean =>
      (!s(p, "client_id") || s(p, "client_id") === clientId) &&
      (!s(p, "source") || s(p, "source") === backend);

    const offQueued = host.on(backend, "queued", (p) => {
      if (mine(p) && s(p, "send_id")) bindNextLocalToSendId(s(p, "send_id"));
    });
    const offStatus = host.on(backend, "status", (p) => {
      if (s(p, "phase") === "done") inquiriesSoon();
      if (!mine(p)) return;
      const phase = s(p, "phase");
      const detail = obj(p, "detail");
      const sendId = s(detail, "send_id");
      if (phase === "queued") {
        bindNextLocalToSendId(sendId);
        setPhase("queued", typeof detail["ahead"] === "number" ? `${detail["ahead"]} ahead` : "");
      } else if (phase === "thinking" || phase === "streaming") {
        if (inflight === null || inflight.send_id !== sendId) promoteToInflight(sendId, "");
        setPhase(phase, "press stop or esc to stop");
      } else if (phase === "tool_calling") {
        handleToolCall(obj(detail, "tool"));
        setPhase("tool_calling", "press stop or esc to stop");
      } else if (phase === "done") {
        // the turn is over: nothing is in flight any more (the `done` event is
        // not always delivered to this client — measured: the button stayed on
        // "stop" after the answer)
        if (inflight !== null && (!sendId || inflight.send_id === sendId)) inflight = null;
        showStats(detail);
        refreshBusy();
        setPhase("idle", s(detail, "reason") && s(detail, "reason") !== "ok" ? s(detail, "reason") : "", s(detail, "reason") || "ok");
        // a turn that failed (not one stopped): the agent may not be set up right
        const why = s(detail, "reason");
        // (`awaiting` is a chain waiting on its calls — an inquiry, a job — not a failure)
        if (why && why !== "ok" && why !== "awaiting" && !/cancel|stop|interrupt|abort/i.test(why)) setupHelp();
      }
    });
    // THE PARTS: one draft per stream_id. ANIMATION, NOT ACTION — a delta never
    // acts; the part's end settles it (a malformed call, `ok:false`, was never
    // dispatched), an abort settles it as NOT SENT. Reasoning stays masked.
    const drafts = new Map<string, Draft>();
    // STREAMING IS FOR THE HUMAN (owner, 2026-10-04): a part is drawn only when it
    // answers THIS client; a message between agents is read whole, as its row.
    const toMe = (p: Dict): boolean => s(p, "client_id") === clientId && (!s(p, "source") || s(p, "source") === backend);
    const callHead = (p: Dict): string => `→ ${s(p, "target") || (Array.isArray(p["to"]) ? (p["to"] as string[]).join(", ") : "")} ${s(p, "name")}`.trim();
    /** Settle a draft as NOT SENT, saying why. */
    const settle = (id: string, why: string): void => {
      const d = drafts.get(id);
      if (d === undefined) return;
      drafts.delete(id);
      if (d.kind === "thinking" && d.text === "") {
        d.el.remove();
        return;
      }
      d.el.classList.remove("thinking");
      d.el.classList.add("not-sent");
      d.el.dataset["why"] = why;
      if (d.kind === "thinking") {
        const sum = d.el.querySelector("summary");
        if (sum) sum.textContent = "thinking";
      }
      d.el.title = `not sent — ${why}`;
    };
    const offStart = host.on(backend, "part_start", (p) => {
      if (!toMe(p)) return;
      const id = s(p, "stream_id");
      const kind = s(p, "kind");
      // THINKING is a collapsible block (owner 2026-10-12): it streams in, stays
      // after, collapsed — click to expand
      const el = document.createElement(kind === "thinking" ? "details" : "div");
      el.className = kind === "call" ? "msg tool" : kind === "thinking" ? "msg assistant thinking c-think" : "msg assistant";
      el.dataset["stream"] = id;
      if (typeof p["re"] === "number") el.dataset["re"] = String(p["re"]);
      if (kind === "thinking") {
        el.innerHTML = `<summary>thinking</summary><div class="c-think-body"></div>`;
        el.dataset["t0"] = String(Date.now());
      } else el.textContent = kind === "call" ? callHead(p) : "";
      dialogEl.appendChild(el);
      dialogEl.scrollTop = dialogEl.scrollHeight;
      drafts.set(id, { el, kind, text: "" });
    });
    const offDelta = host.on(backend, "part_delta", (p) => {
      if (!toMe(p)) return;
      if (genStart === 0) genStart = Date.now();
      genChars += s(p, "text").length;
      const d = drafts.get(s(p, "stream_id"));
      if (d === undefined) return;
      d.text += s(p, "text");
      if (d.kind === "thinking") {
        const body = d.el.querySelector(".c-think-body");
        if (body) body.textContent = d.text;
      }
      if (d.kind === "text") {
        d.el.textContent = d.text;
        dialogEl.scrollTop = dialogEl.scrollHeight;
      }
    });
    const offEnd = host.on(backend, "part_end", (p) => {
      if (!toMe(p)) return;
      const id = s(p, "stream_id");
      const d = drafts.get(id);
      if (d === undefined) return;
      if (p["ok"] === false) {
        settle(id, "malformed");
        return;
      }
      drafts.delete(id);
      // a finished thought stays, collapsed: how long it took; click to read it
      if (d.kind === "thinking") {
        if (d.text.trim() === "") d.el.remove();
        else {
          d.el.classList.remove("thinking");
          const secs = ((Date.now() - Number(d.el.dataset["t0"] ?? Date.now())) / 1000).toFixed(1);
          const sum = d.el.querySelector("summary");
          if (sum) sum.textContent = `thought for ${secs}s`;
        }
      }
    });
    const offAbort = host.on(backend, "part_abort", (p) => {
      if (toMe(p)) settle(s(p, "stream_id"), s(p, "reason") || "aborted");
    });
    const offDone = host.on(backend, "done", (p) => {
      inquiriesSoon();
      if (mine(p)) {
        inflight = null;
        refreshBusy();
      }
    });
    // context protocol — push half. Render an inline centered notice when the live view
    // was compacted, or when the window is too small (a failfast; the model wasn't called).
    const offContext = host.on(backend, "context", (p) => {
      if (!mine(p)) return;
      const phase = s(p, "phase");
      const detail = obj(p, "detail");
      let bubble: HTMLElement;
      if (phase === "too_small") {
        bubble = addMsg("notice", `context too small — ${s(detail, "hint")}`, dialogEl);
        bubble.classList.add("too-small");
        setPhase("idle", "context too small");
        refreshBusy();
      } else {
        // ONLY WHEN SOMETHING WENT (owner 2026-10-11): past the fold trigger the
        // projection runs every turn, mostly stripping repeats — that is no news
        const strat = s(detail, "strategy");
        const dropped = typeof detail["dropped_messages"] === "number" ? (detail["dropped_messages"] as number) : 0;
        const folded = typeof detail["folded"] === "number" ? (detail["folded"] as number) : 0;
        const kept = typeof detail["kept_messages"] === "number" ? (detail["kept_messages"] as number) : 0;
        if (folded === 0 && dropped === 0) return;
        addMsg("notice", folded > 0
          ? `context folded · ${folded} older part${folded === 1 ? "" : "s"} summarized · ${kept} kept (recall to page back)`
          : `context compacted · ${strat} · ${dropped} dropped → ${kept} kept (recall to page back)`, dialogEl);
      }
    });

    // boot: replay history + rebuild from a status snapshot
    // `history` answers at DIGEST resolution ({id, role, head, bytes} per row) on
    // every runtime; a row's full text is behind the agent's recall / grep.
    let lastRow = 0; // the newest history row this cell has drawn
    // the NEWEST rows only: history pages oldest-first; historyTail keeps the last 300
    // (a backend not answering yet answers null: no rows)
    const tail = await historyTail((p) => host.call(backend, { ...p, client_id: clientId } as never) as Promise<Dict | null>);
    const rows = tail.rows as Dict[];
    // THINKING, kept on its answer's row (`thinking`, flagged `has_thinking` in the
    // digest): the full rows are read once, and each answer is drawn after its
    // thought — collapsed. The pager starts just before the first kept row.
    const thoughts = new Map<number, { text: string; cut: number }>();
    if (rows.some((m) => m["has_thinking"] === true)) {
      let after: unknown = typeof rows[0]?.["id"] === "number" ? (rows[0]!["id"] as number) - 1 : undefined;
      for (let page = 0; page < 20; page++) {
        const r = (await host.call(backend, { type: "rows", ...(after !== undefined ? { after } : {}) } as never).catch(() => null)) as Dict | null;
        const got = r !== null && Array.isArray(r["rows"]) ? (r["rows"] as Dict[]) : [];
        for (const x of got) if (typeof x["id"] === "number" && typeof x["thinking"] === "string") thoughts.set(x["id"] as number, { text: x["thinking"] as string, cut: typeof x["thinking_cut"] === "number" ? (x["thinking_cut"] as number) : 0 });
        if (r === null || r["next"] === null || r["next"] === undefined) break;
        after = r["next"];
      }
    }
    // `kind: "notice"` rows are the AI layer's own turns to its model: not chat
    // — except the checker's report (isReport), which repeats fold to one ×N row
    // CARDS: each agent on show is drawn at ITS embed row's place (a later unembed took it out)
    const shown = cardRows(rows);
    for (const m of collapseReports(rows, isReport)) {
      if (isCardRow(m)) {
        const agent = s(m, "head");
        if (s(m, "kind") === "embed" && shown.get(agent) === m["id"]) dialogEl.appendChild(cards.draw(agent));
        continue;
      }
      const t = typeof m["id"] === "number" ? thoughts.get(m["id"] as number) : undefined;
      if (t !== undefined) dialogEl.appendChild(thoughtBlock(t.text, t.cut));
      if (s(m, "kind") !== "notice" || isReport(m)) {
        const rep = typeof m["repeat"] === "number" ? ` ×${m["repeat"]}` : "";
        addMsg(s(m, "role"), s(m, "head") + rep, dialogEl);
      }
    }
    lastRow = tail.lastId;
    convBytes += tail.bytes;
    const snap = (await host.call(backend, { type: "status", client_id: clientId })) as Dict;
    if (snap) {
      setOthersHint(typeof snap["others_pending"] === "number" ? (snap["others_pending"] as number) : 0);
      const cur = obj(snap, "current");
      if (cur["is_mine"] === true) {
        const ub = addMsg("user", s(cur, "text"), dialogEl);
        if (s(cur, "text_so_far") !== "") addMsg("assistant", s(cur, "text_so_far"), dialogEl);
        inflight = { send_id: s(cur, "send_id"), userBubble: ub, toolBlocks: new Map() };
        const lastTool = obj(cur, "last_tool");
        if (Object.keys(lastTool).length > 0) {
          const h = makeToolBlock(lastTool);
          inflight.toolBlocks.set(s(lastTool, "call_id"), h);
          dialogEl.appendChild(h.el);
          if ("reply_preview" in lastTool) fillToolReply(h, s(lastTool, "reply_preview"));
        }
        setPhase(s(cur, "phase") || "thinking", "press stop or esc to stop");
      }
      const pending = Array.isArray(snap["mine_pending"]) ? (snap["mine_pending"] as Dict[]) : [];
      for (const entry of pending) {
        const bubble = addMsg("user", s(entry, "text"), queuedEl);
        bubble.classList.add("queued");
        queuedBubbles.set(s(entry, "send_id"), bubble);
        queuedOrder.push(s(entry, "send_id"));
      }
      refreshBusy();
    }

    // A cell's turns are typed at a SPOT: its own position unless a first turn
    // names one (the action popup's). Plain chat views (not cells) send plain text.
    const isCell = tpl !== "";
    const cellSpot = (): [number, number] => {
      // where it sits in the theme in use (its view's layout)
      const view = kernel.get(selfId)?.parentId ?? "";
      const r = rectsOf(kernel, view, themeOf(kernel, view, innerWidth))[selfId];
      return [Math.round(r?.x ?? 0), Math.round(r?.y ?? 0)];
    };
    const submit = (override?: string, at?: [number, number]): void => {
      const typed = (override ?? inputEl.value).trim();
      if (!typed) return;
      if (override === undefined) inputEl.value = "";
      pinned = true;
      toBottom();
      const [sx, sy] = at ?? cellSpot();
      // the setup rides the dialog's FIRST turn only (the transcript keeps it)
      const first = isCell && !dialogEl.querySelector(".msg.user") && queuedOrder.length === 0;
      const text = isCell ? `${typed}${first ? `\n${CANVAS_SETUP.replaceAll("{{node}}", canvasNode())}` : ""}\n[at ${sx},${sy}]` : typed;
      const localKey = "local_" + Math.random().toString(36).slice(2, 10);
      const bubble = addMsg("user", text, queuedEl);
      bubble.classList.add("queued");
      queuedBubbles.set(localKey, bubble);
      queuedOrder.push(localKey);
      localPending.push(localKey);
      refreshBusy();
      const payload: Dict = { type: "send", text, client_id: clientId };
      const dropLocal = (): void => {
        queuedBubbles.get(localKey)?.remove();
        queuedBubbles.delete(localKey);
        const i = queuedOrder.indexOf(localKey);
        if (i >= 0) queuedOrder.splice(i, 1);
        const j = localPending.indexOf(localKey);
        if (j >= 0) localPending.splice(j, 1);
        refreshBusy();
      };
      void host.call(backend, payload as never).then((r) => {
        // A REFUSAL IS DATA: shown in the dialog, in the kernel's words.
        const o = r as Dict | null;
        if (o && typeof o["error"] === "string") {
          const hint = typeof o["hint"] === "string" && o["hint"] ? ` — ${o["hint"]}` : "";
          const n = addMsg("notice", `✕ ${o["error"]}${hint}`, dialogEl);
          n.classList.add("too-small");
          setupHelp();
          dropLocal();
        }
      }).catch((e: Error) => {
        addMsg("assistant", "ERROR: " + e.message, dialogEl);
        queuedBubbles.get(localKey)?.remove();
        queuedBubbles.delete(localKey);
        const i = queuedOrder.indexOf(localKey);
        if (i >= 0) queuedOrder.splice(i, 1);
        const j = localPending.indexOf(localKey);
        if (j >= 0) localPending.splice(j, 1);
        refreshBusy();
      });
    };
    // STOP: the backend's own verb — the generation is cancelled (provider stream
    // closed), its open parts abort, nothing is sent; the model's history keeps
    // a cut marker, never a half answer.
    const interrupt = (): void => {
      void host.call(backend, { type: "stop" } as never).catch((e: Error) => console.warn("[chat] stop failed:", e));
    };

    // a kept thought, collapsed: click to read it
    function thoughtBlock(text: string, cut: number): HTMLElement {
      const d = document.createElement("details");
      d.className = "msg assistant c-think";
      const sum = document.createElement("summary");
      sum.textContent = `thought · ${text.length} chars${cut > 0 ? ` (+${cut} cut)` : ""}`;
      const body = document.createElement("div");
      body.className = "c-think-body";
      body.textContent = text;
      d.append(sum, body);
      return d;
    }
    // ITS QUESTIONS TO THE PERSON (owner 2026-10-11): an `inquiry` the AI sent
    // `user` is HELD there; this cell shows its open ones as cards and answers one
    // with `answer_inquiry` — the AI gets it on its thread. Read when its phase
    // changes or a turn ends (an inquiry is sent inside a turn), and at mount.
    const shownInquiries = new Set<number>();
    const optionsOf = (o: unknown): Array<{ value: string; label: string }> =>
      (Array.isArray(o) ? o : []).map((x) => {
        if (x !== null && typeof x === "object") {
          const r = x as Dict;
          const v = String(r["value"] ?? r["label"] ?? "");
          return { value: v, label: String(r["label"] ?? v) };
        }
        return { value: String(x), label: String(x) };
      });
    const inquiryCard = (q: Dict): HTMLElement => {
      const re = Number(q["re"]);
      const kind = s(q, "kind") || "text";
      const card = document.createElement("div");
      card.className = "msg c-inquiry";
      card.dataset.re = String(re);
      const head = document.createElement("div");
      head.className = "c-inq-head";
      head.textContent = "asks you";
      const text = document.createElement("div");
      text.className = "c-inq-text";
      text.textContent = s(q, "text");
      const row = document.createElement("div");
      row.className = "c-inq-row";
      const status = document.createElement("div");
      status.className = "c-inq-status";
      card.append(head, text, row, status);
      const reply = async (answer: unknown, shown: string): Promise<void> => {
        for (const b of row.querySelectorAll("button, input")) (b as HTMLButtonElement).disabled = true;
        const r = (await host.callHost("user", { type: "answer_inquiry", re, answer } as never).catch((e: Error) => ({ error: e.message }))) as Dict;
        if (typeof r["error"] === "string") {
          status.textContent = `✕ ${r["error"]}`;
          for (const b of row.querySelectorAll("button, input")) (b as HTMLButtonElement).disabled = false;
          return;
        }
        card.classList.add("answered");
        row.replaceChildren();
        status.textContent = `✓ you answered: ${shown}`;
      };
      const button = (label: string, onClick: () => void, primary = false): HTMLButtonElement => {
        const b = document.createElement("button");
        b.textContent = label;
        if (primary) b.className = "primary";
        b.onclick = onClick;
        return b;
      };
      if (kind === "confirm") {
        row.append(button("Yes", () => void reply(true, "yes"), true), button("No", () => void reply(false, "no")));
      } else if (kind === "single") {
        for (const o of optionsOf(q["options"])) row.append(button(o.label, () => void reply(o.value, o.label)));
      } else if (kind === "multi") {
        const picks: HTMLInputElement[] = [];
        for (const o of optionsOf(q["options"])) {
          const l = document.createElement("label");
          const c = document.createElement("input");
          c.type = "checkbox";
          c.value = o.value;
          picks.push(c);
          l.append(c, ` ${o.label}`);
          row.append(l);
        }
        row.append(button("answer", () => {
          const chosen = picks.filter((c) => c.checked).map((c) => c.value);
          if (chosen.length > 0) void reply(chosen, chosen.join(", "));
        }, true));
      } else {
        const input = document.createElement("input");
        input.className = "c-inq-input";
        input.placeholder = "your answer…";
        const send = (): void => {
          if (input.value.trim()) void reply(input.value.trim(), input.value.trim());
        };
        input.onkeydown = (e: KeyboardEvent): void => {
          if (e.key === "Enter") send();
        };
        row.append(input, button("answer", send, true));
      }
      return card;
    };
    // LIVE: a checker report is a turn the AI layer delivers, never streamed —
    // read once a turn ends: the rows after the last one seen (`lastRow`)
    const rowsOf = (h: Dict | null): Dict[] => (h !== null && Array.isArray(h["rows"]) ? (h["rows"] as Dict[]) : []);
    const checkReports = async (): Promise<void> => {
      const read = (args: Dict): Promise<Dict | null> =>
        host.call(backend, { type: "history", client_id: clientId, ...args } as never).catch(() => null) as Promise<Dict | null>;
      // a cursor the history no longer holds (a clear started a new thread) pages
      // nothing: then the thread from its start, keeping only what is newer
      let fresh = lastRow > 0 ? rowsOf(await read({ after: lastRow })) : [];
      if (fresh.length === 0) fresh = rowsOf(await read({})).filter((m) => typeof m["id"] === "number" && (m["id"] as number) > lastRow);
      for (const m of fresh) {
        if (typeof m["id"] === "number") lastRow = Math.max(lastRow, m["id"] as number);
        if (typeof m["bytes"] === "number") convBytes += m["bytes"] as number;
        if (isCardRow(m)) applyCardRow(m);
        else if (s(m, "head").includes("[malformed call — nothing ran]")) addMsg("user", s(m, "head"), dialogEl);
      }
      showStats({});
    };
    const checkInquiries = async (): Promise<void> => {
      const r = (await host.callHost("user", { type: "inquiries", asker: backend } as never).catch(() => null)) as Dict | null;
      const open = r !== null && Array.isArray(r["inquiries"]) ? (r["inquiries"] as Dict[]) : [];
      for (const q of open) {
        const re = Number(q["re"]);
        if (!Number.isFinite(re) || shownInquiries.has(re)) continue;
        shownInquiries.add(re);
        dialogEl.appendChild(inquiryCard(q));
      }
    };
    // read once per turn's end (an inquiry ends its turn: awaited or done), debounced
    let inquiryTimer: ReturnType<typeof setTimeout> | null = null;
    const inquiriesSoon = (): void => {
      if (inquiryTimer !== null) return;
      inquiryTimer = setTimeout(() => {
        inquiryTimer = null;
        void checkInquiries();
        void checkReports();
      }, 250);
    };
    // the card rows after the last row seen, read soon (a burst of tool replies is one read)
    let cardTimer: ReturnType<typeof setTimeout> | null = null;
    cardsSoonImpl = (): void => {
      if (cardTimer !== null) return;
      cardTimer = setTimeout(() => {
        cardTimer = null;
        void checkReports();
      }, 60);
    };
    // A TURN THAT FAILED: how to fix the agent — its settings, or a coding agent
    // started next to it to help (owner 2026-10-11)
    const setupHelp = (): void => {
      if (ctx.actions === undefined || dialogEl.querySelector(".c-setup-help")) return;
      const help = document.createElement("div");
      help.className = "msg notice c-setup-help";
      help.innerHTML = `Press settings to set me up, or start your coding AI agent here to help set me up.<div class="c-help-row"><button class="c-help-settings">⚙ settings</button><button class="c-help-coding">▸ coding agent</button></div>`;
      (help.querySelector(".c-help-settings") as HTMLButtonElement).onclick = (): void => ctx.actions?.openSetup(selfId, backend);
      (help.querySelector(".c-help-coding") as HTMLButtonElement).onclick = (): void => void ctx.actions?.startCoding(selfId);
      dialogEl.appendChild(help);
    };
    sendBtn.addEventListener("click", () => (inflight !== null ? interrupt() : submit()));
    // The action popup's text is the cell's FIRST turn: sent once, then dropped
    // from the record so a reload does not send it again.
    const first = record?.["first_turn"] as Dict | undefined;
    if (first && typeof first["text"] === "string" && dialogEl.childElementCount === 0) {
      const at: [number, number] = [Math.round(Number(first["x"] ?? 0)), Math.round(Number(first["y"] ?? 0))];
      kernel.updateMeta(selfId, { first_turn: null } as never);
      submit(first["text"] as string, at);
    }
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        submit();
      }
    };
    inputEl.addEventListener("keydown", onKey);
    const onEsc = (e: KeyboardEvent): void => {
      if (e.key === "Escape" && inflight !== null) {
        e.preventDefault();
        interrupt();
      }
    };
    window.addEventListener("keydown", onEsc);
    // mounted and listening: the input takes a turn from here on
    root.dataset.ready = "1";
    void checkInquiries();

    return {
      // ⚙ SETTINGS: the wizard's page 2 for this agent
      headerButtons: ctx.actions
        ? [{ id: "settings", glyph: "⚙", title: "settings — this agent's provider, model and key", toggle: false, onClick: (): void => ctx.actions?.openSetup(selfId, backend) }]
        : [],
      unmount(): void {
        if (cardTimer !== null) clearTimeout(cardTimer);
        stick.disconnect();
        offLifecycle();
        offQueued();
        offStatus();
        offStart();
        offDelta();
        offEnd();
        offAbort();
        offDone();
        offContext();
        offState();
        cards.dispose();
        host.unwatch(backend);
        window.removeEventListener("keydown", onEsc);
        if (elapsedTimer !== null) clearInterval(elapsedTimer);
        root.remove();
      },
    };
  },
};
