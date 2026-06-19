import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { Host } from "../host.ts";

// ai_view — inline, MULTI-MODE AI view (chat mode = provider-agnostic chat UI). Ported from
// ai_chat_webapp/index.html, rendered INLINE into a canvas frame, fronting any
// backend that answers send/history/interrupt/status (ollama_backend,
// nvidia_nim_backend). Preserves the full queue/inflight state machine, tool
// blocks, status phases, history+snapshot rebuild, lifecycle reset, ESC-interrupt.
// ids → scoped classes so many chat frames coexist on one canvas page.

const CHAT_CSS = `
  .chat-view { display:flex; flex-direction:column; height:100%; font-family:system-ui,sans-serif; color:#e0e0ff; background:transparent; box-sizing:border-box; }
  .chat-view .c-header { padding:8px 14px; font-size:12px; color:rgba(220,230,245,0.65); border-bottom:1px solid rgba(255,255,255,0.06); display:flex; justify-content:space-between; }
  .chat-view .c-header b { color:rgba(220,230,245,0.95); }
  .chat-view .c-dialog { flex:1; overflow-y:auto; padding:12px; display:flex; flex-direction:column; gap:8px; }
  .chat-view .msg { padding:8px 12px; border-radius:12px; line-height:1.45; max-width:85%; word-wrap:break-word; white-space:pre-wrap; font-size:14px; backdrop-filter:blur(20px) saturate(160%); -webkit-backdrop-filter:blur(20px) saturate(160%); box-shadow:inset 0 1px 0 rgba(255,255,255,0.10),0 0 0 1px rgba(255,255,255,0.08),0 6px 14px -6px rgba(0,0,0,0.45); }
  .chat-view .msg.user { align-self:flex-end; background:rgba(110,90,220,0.32); color:#fff; }
  .chat-view .msg.user.queued { opacity:0.55; font-style:italic; }
  .chat-view .msg.assistant { align-self:flex-start; background:rgba(36,38,56,0.50); color:#cde; }
  .chat-view .msg.notice { align-self:center; max-width:92%; text-align:center; font-size:11px; color:#ffd166; background:rgba(255,209,102,0.12); box-shadow:inset 0 0 0 1px rgba(255,209,102,0.18); }
  .chat-view .msg.notice.too-small { color:#ff8a8a; background:rgba(255,120,120,0.12); box-shadow:inset 0 0 0 1px rgba(255,120,120,0.22); }
  .chat-view .tool-block.recall .tool-verb { color:#9ad0ff; }
  .chat-view .tool-block { margin:6px 0; padding:6px 8px; border-radius:8px; background:rgba(24,26,40,0.50); backdrop-filter:blur(12px) saturate(160%); box-shadow:inset 0 0 0 1px rgba(255,255,255,0.06); font-family:ui-monospace,SFMono-Regular,monospace; font-size:12px; color:#abb; }
  .chat-view .tool-block .tool-head { display:flex; align-items:baseline; gap:6px; }
  .chat-view .tool-block .tool-verb { color:#ffd166; }
  .chat-view .tool-block .tool-target { color:#8ad; }
  .chat-view .tool-block .tool-state { margin-left:auto; color:#666; font-size:11px; }
  .chat-view .tool-block.pending .tool-state::after { content:'…'; animation:cpulse 1.4s ease-in-out infinite; display:inline-block; }
  .chat-view .tool-block .tool-detail summary { cursor:pointer; color:#667; font-size:11px; }
  .chat-view .tool-block pre { margin:4px 0; padding:6px; background:rgba(8,10,18,0.55); border-radius:4px; color:#abb; white-space:pre-wrap; word-break:break-all; max-height:160px; overflow-y:auto; }
  .chat-view .c-queued { display:flex; flex-direction:column; gap:4px; padding:0 12px; }
  .chat-view .c-queued:empty { display:none; }
  .chat-view .c-queued .msg.user { max-width:100%; opacity:0.55; font-style:italic; }
  .chat-view .c-footer { display:flex; align-items:center; gap:10px; padding:6px 14px; border-top:1px solid rgba(255,255,255,0.06); font-size:11px; color:rgba(220,230,245,0.55); min-height:28px; }
  .chat-view .phase-pill { display:inline-block; padding:2px 8px; border-radius:999px; background:rgba(255,255,255,0.08); color:rgba(220,230,245,0.75); font-weight:500; }
  .chat-view .phase-pill[data-phase="thinking"],.chat-view .phase-pill[data-phase="tool_calling"] { background:rgba(110,90,220,0.45); color:#fff; animation:cpulse 1.4s ease-in-out infinite; }
  .chat-view .phase-pill[data-phase="streaming"] { background:rgba(80,180,120,0.40); color:#fff; }
  .chat-view .phase-pill[data-phase="queued"] { background:rgba(255,209,102,0.20); color:#ffd166; }
  .chat-view .others-hint { color:#666; font-size:10px; }
  @keyframes cpulse { 0%,100%{opacity:1;} 50%{opacity:0.45;} }
  .chat-view .c-row { display:flex; padding:10px; gap:8px; border-top:1px solid rgba(255,255,255,0.06); }
  .chat-view .c-input { flex:1; background:rgba(255,255,255,0.06); backdrop-filter:blur(16px) saturate(180%); border:0; box-shadow:inset 0 0 0 1px rgba(255,255,255,0.10); border-radius:10px; padding:8px 12px; color:#e0e0ff; font-size:14px; outline:none; }
  .chat-view .c-input:focus { box-shadow:inset 0 0 0 1px rgba(180,220,255,0.45),0 0 0 3px rgba(120,200,255,0.10); }
  .chat-view .c-send { background:rgba(110,90,220,0.45); border:0; box-shadow:inset 0 1px 0 rgba(255,255,255,0.12),inset 0 0 0 1px rgba(255,255,255,0.10); border-radius:10px; color:#fff; padding:8px 14px; cursor:pointer; }
  .chat-view .c-send:hover { background:rgba(130,110,240,0.55); }
  .chat-view .c-send.stop { background:rgba(200,80,80,0.55); }
`;

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
  assistantBubble: HTMLElement;
  toolBlocks: Map<string, ToolHandle>;
  streamText: string;
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
  // host AI/chat backend — anthropic / ollama / nvidia). A `mode` field on the
  // record selects the renderer: `chat` is fully wired; `agent-prompt` and
  // `dynamic-buttons` are scaffolded seams (mountStub) for later.
  handles: ["ai_view.ts"],

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

    // client_id: stable per backend, localStorage so refresh keeps the thread
    const CLIENT_KEY = `ai_chat_client_id:${backend}`;
    let clientId = localStorage.getItem(CLIENT_KEY);
    if (clientId === null) {
      clientId = "web_" + Math.random().toString(36).slice(2, 10);
      localStorage.setItem(CLIENT_KEY, clientId);
    }
    $<HTMLElement>(".c-agent").textContent = `${selfId} (client ${clientId})`;
    const upRefl = (await host.call(backend, { type: "reflect" })) as Dict;
    $<HTMLElement>(".c-model").textContent = s(upRefl, "model");

    const addMsg = (role: string, text: string, parent: HTMLElement): HTMLElement => {
      const div = document.createElement("div");
      div.className = "msg " + role;
      div.textContent = text;
      parent.appendChild(div);
      if (parent === dialogEl) dialogEl.scrollTop = dialogEl.scrollHeight;
      return div;
    };

    const queuedBubbles = new Map<string, HTMLElement>();
    const queuedOrder: string[] = [];
    const localPending: string[] = [];
    let inflight: Inflight | null = null;

    const refreshBusy = (): void => {
      const on = inflight !== null || queuedBubbles.size > 0;
      sendBtn.textContent = on ? "stop" : "send";
      sendBtn.classList.toggle("stop", on);
    };

    let elapsedTimer: ReturnType<typeof setInterval> | null = null;
    let elapsedStart = 0;
    const setPhase = (phase: string, hint = ""): void => {
      phasePill.dataset["phase"] = phase;
      phasePill.textContent = phase;
      hintEl.textContent = hint;
      const live = phase === "thinking" || phase === "streaming" || phase === "tool_calling" || phase === "queued";
      if (live) {
        if (elapsedTimer === null) {
          elapsedStart = Date.now();
          elapsedTimer = setInterval(() => {
            elapsedEl.textContent = `${((Date.now() - elapsedStart) / 1000).toFixed(1)}s`;
          }, 250);
        }
      } else if (elapsedTimer !== null) {
        clearInterval(elapsedTimer);
        elapsedTimer = null;
        elapsedEl.textContent = "";
      }
    };
    const setOthersHint = (n: number): void => {
      othersHintEl.textContent = n > 0 ? `+${n} from other clients` : "";
    };
    setPhase("idle");

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
        bubble = addMsg("user", fallback, dialogEl);
      } else {
        bubble.classList.remove("queued");
        bubble.style.maxWidth = "";
        bubble.style.opacity = "";
        dialogEl.appendChild(bubble);
        queuedBubbles.delete(sendId);
        const i = queuedOrder.indexOf(sendId);
        if (i >= 0) queuedOrder.splice(i, 1);
      }
      inflight = {
        send_id: sendId,
        userBubble: bubble,
        assistantBubble: addMsg("assistant", "", dialogEl),
        toolBlocks: new Map(),
        streamText: "",
      };
      refreshBusy();
    };

    const handleToolCall = (tool: Dict): void => {
      if (inflight === null) return;
      const callId = s(tool, "call_id");
      let handle = inflight.toolBlocks.get(callId);
      if (!handle) {
        handle = makeToolBlock(tool);
        inflight.toolBlocks.set(callId, handle);
        inflight.assistantBubble.appendChild(handle.el);
        dialogEl.scrollTop = dialogEl.scrollHeight;
      }
      if ("reply_preview" in tool) fillToolReply(handle, s(tool, "reply_preview"));
    };

    // lifecycle: drop stale inflight on disconnect (no `done` will arrive)
    const offLifecycle = host.onLifecycle(backend, (state) => {
      if (state === "disconnected") {
        inflight = null;
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
      if (!mine(p)) return;
      const phase = s(p, "phase");
      const detail = obj(p, "detail");
      const sendId = s(detail, "send_id");
      if (phase === "queued") {
        bindNextLocalToSendId(sendId);
        setPhase("queued", typeof detail["ahead"] === "number" ? `${detail["ahead"]} ahead` : "");
      } else if (phase === "thinking" || phase === "streaming") {
        if (inflight === null || inflight.send_id !== sendId) promoteToInflight(sendId, "");
        setPhase(phase, "press stop or esc to interrupt");
      } else if (phase === "tool_calling") {
        handleToolCall(obj(detail, "tool"));
        setPhase("tool_calling", "press stop or esc to interrupt");
      } else if (phase === "done") {
        setPhase("idle", s(detail, "reason") && s(detail, "reason") !== "ok" ? s(detail, "reason") : "");
      }
    });
    const offToken = host.on(backend, "token", (p) => {
      if (mine(p) && inflight !== null) {
        inflight.streamText += s(p, "text");
        inflight.assistantBubble.textContent = inflight.streamText;
        dialogEl.scrollTop = dialogEl.scrollHeight;
      }
    });
    const offDone = host.on(backend, "done", (p) => {
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
        const strat = s(detail, "strategy");
        const dropped = typeof detail["dropped_turns"] === "number" ? (detail["dropped_turns"] as number) : 0;
        const kept = typeof detail["kept_turns"] === "number" ? (detail["kept_turns"] as number) : 0;
        addMsg("notice", `context compacted · ${strat} · ${dropped} dropped → ${kept} kept (recall to page back)`, dialogEl);
      }
    });

    // boot: replay history + rebuild from a status snapshot
    const hist = (await host.call(backend, { type: "history", client_id: clientId })) as Dict;
    const messages = Array.isArray(hist["messages"]) ? (hist["messages"] as Dict[]) : [];
    for (const m of messages) addMsg(s(m, "role"), s(m, "content"), dialogEl);
    const snap = (await host.call(backend, { type: "status", client_id: clientId })) as Dict;
    if (snap) {
      setOthersHint(typeof snap["others_pending"] === "number" ? (snap["others_pending"] as number) : 0);
      const cur = obj(snap, "current");
      if (cur["is_mine"] === true) {
        const ub = addMsg("user", s(cur, "text"), dialogEl);
        const ab = addMsg("assistant", s(cur, "text_so_far"), dialogEl);
        inflight = { send_id: s(cur, "send_id"), userBubble: ub, assistantBubble: ab, toolBlocks: new Map(), streamText: s(cur, "text_so_far") };
        const lastTool = obj(cur, "last_tool");
        if (Object.keys(lastTool).length > 0) {
          const h = makeToolBlock(lastTool);
          inflight.toolBlocks.set(s(lastTool, "call_id"), h);
          ab.appendChild(h.el);
          if ("reply_preview" in lastTool) fillToolReply(h, s(lastTool, "reply_preview"));
        }
        setPhase(s(cur, "phase") || "thinking", "press stop or esc to interrupt");
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

    const submit = (): void => {
      const text = inputEl.value.trim();
      if (!text) return;
      inputEl.value = "";
      const localKey = "local_" + Math.random().toString(36).slice(2, 10);
      const bubble = addMsg("user", text, queuedEl);
      bubble.classList.add("queued");
      queuedBubbles.set(localKey, bubble);
      queuedOrder.push(localKey);
      localPending.push(localKey);
      refreshBusy();
      void host.call(backend, { type: "send", text, client_id: clientId }).catch((e: Error) => {
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
    const interrupt = (): void => {
      void host.call(backend, { type: "interrupt" }).catch((e: Error) => console.warn("[chat] interrupt failed:", e));
    };

    sendBtn.addEventListener("click", () => (inflight !== null ? interrupt() : submit()));
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

    return {
      unmount(): void {
        offLifecycle();
        offQueued();
        offStatus();
        offToken();
        offDone();
        offContext();
        host.unwatch(backend);
        window.removeEventListener("keydown", onEsc);
        if (elapsedTimer !== null) clearInterval(elapsedTimer);
        root.remove();
      },
    };
  },
};
