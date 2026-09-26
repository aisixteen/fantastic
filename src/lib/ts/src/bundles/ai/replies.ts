import type { Json, Payload } from "../../kernel/json.ts";
import type { Turn } from "./context.ts";

// REPLY ROWS — the ts counterpart of python's `[reply to <pid> · <target> <verb>]`
// tag. The model reads every reply as `<tool_response>…</tool_response>` (the ts
// tool grammar, unchanged); the stored row carries the call it answers as
// metadata — `reply: {pid, target, verb}` — which is what python parses out of
// its tag. Everything that works per reply (strip, repeat/stale dedupe, grep's
// descent, the recall prompt) reads that metadata. One reply per row: the ts loop
// answers each call with its own turn.

export const RESPONSE_OPEN = "<tool_response>";
export const RESPONSE_CLOSE = "</tool_response>";

export interface ReplyMeta {
  pid: number | null;
  target: string;
  verb: string;
}

export function isReply(m: Turn): boolean {
  return m["role"] === "user" && typeof m["content"] === "string" && m["content"].startsWith(RESPONSE_OPEN);
}

export function replyMeta(m: Turn): ReplyMeta | null {
  const r = m["reply"];
  if (r === null || typeof r !== "object" || Array.isArray(r)) return null;
  return {
    pid: typeof r["pid"] === "number" ? r["pid"] : null,
    target: String(r["target"] ?? "?"),
    verb: String(r["verb"] ?? "?"),
  };
}

/** The text between the reply's tags. */
export function replyBody(content: string): string {
  let s = content.startsWith(RESPONSE_OPEN) ? content.slice(RESPONSE_OPEN.length) : content;
  if (s.endsWith(RESPONSE_CLOSE)) s = s.slice(0, -RESPONSE_CLOSE.length);
  return s;
}

export function wrap(body: string): string {
  return `${RESPONSE_OPEN}${body}${RESPONSE_CLOSE}`;
}

/** The STUB a stripped reply leaves: which call it answered, and how big the body
 *  was. Deterministic in the row, so the stripped prefix is byte-stable. */
export function stubOf(m: Turn): string {
  const meta = replyMeta(m);
  const body = replyBody(String(m["content"] ?? ""));
  const tag = meta !== null ? `[reply to ${meta.pid ?? "-"} · ${meta.target} ${meta.verb}] ` : "";
  return wrap(`${tag}(stripped: ${body.length} bytes — recall by pid)`);
}

/** The resolution a rendered row is at: a fold, a stripped stub, or the row itself. */
export function resOf(row: Turn): "fold" | "stub" | "full" {
  if (row["kind"] === "fold") return "fold";
  const c = row["content"];
  if (typeof c === "string" && isReply(row) && c.includes("(stripped:")) return "stub";
  return "full";
}

/** Render a stored row at the resolution the view array asks for. */
export function applyRes(row: Turn, res: string): Turn {
  if (res === "stub" && isReply(row)) return { ...row, content: stubOf(row) };
  return row;
}

// ─── the reflect distillation ────────────────────────────────────────────

/** Rides ON a reflect reply (point of use, not policy): the model distils what it
 *  needs in its next message, and the answered body then drops from the view. */
export const REFLECT_NUDGE =
  "\n[this body is TRANSIENT — after your next message it is replaced by a stub. " +
  "Open that message with the facts you will need, one compact line per agent:\n" +
  "SEEN <id> — <what it is for>; verbs: <the ones you intend to use>. " +
  "One reflect per agent is complete — reflecting it again yields nothing new; " +
  "the next useful call is an action.]";

const DISTILLED = /\bSEEN\b/i;

export function distilled(text: string): boolean {
  return DISTILLED.test(text);
}

/**
 * Replace the body of every reflect reply the model has already answered WITH A
 * DISTILLATION (a `SEEN` line in its next assistant turn). FAIL CLOSED: a
 * reflect the model did not distil is kept — non-compliance costs context, never
 * comprehension. The in-flight reflect (not yet answered) stays verbatim. Row
 * keys (id, parent, reply) ride through.
 */
export function dropAnsweredReflects(history: Turn[]): Turn[] {
  return history.map((turn, i) => {
    const meta = isReply(turn) ? replyMeta(turn) : null;
    if (meta === null || meta.verb !== "reflect") return turn;
    const answer = history.slice(i + 1).find((t) => t["role"] === "assistant");
    if (answer === undefined || !distilled(String(answer["content"] ?? ""))) return turn;
    return { ...turn, content: wrap(`[reflect of ${meta.target} — full text dropped; your distillation follows]`) };
  });
}

/** Verbs that return WHOLE state: the newest reply IS the truth, an older one
 *  describes a state that no longer exists. Parameterised reads (`read`, `list`,
 *  `status`, `get`) are never deduped — two of them are different answers. */
export const WHOLE_STATE_READS: ReadonlySet<string> = new Set(["reflect", "list_agents", "ps", "keys"]);

/** Drop whole-state read replies that a later reply to the same (agent, verb)
 *  supersedes. Newest wins; order is otherwise preserved. */
export function dedupeStaleReads(history: Turn[]): Turn[] {
  const seen = new Set<string>();
  const keepRev: Turn[] = [];
  for (let i = history.length - 1; i >= 0; i--) {
    const turn = history[i] as Turn;
    const meta = isReply(turn) ? replyMeta(turn) : null;
    if (meta !== null && WHOLE_STATE_READS.has(meta.verb)) {
      const key = `${meta.target}\u0000${meta.verb}`;
      if (seen.has(key)) continue; // a later reply already carries this state
      seen.add(key);
    }
    keepRev.push(turn);
  }
  return keepRev.reverse();
}

/** A reply whose BODY is identical to an EARLIER one (the same file read again)
 *  is shown as a one-line stub citing the first — the first copy stays whole.
 *  Only bodies worth the saving (≥400 chars), never a digest. The view restarts
 *  at a clear, so a read after a clear is new again. */
export function dedupeRepeatBodies(history: Turn[]): Turn[] {
  const seen = new Map<string, Json>();
  return history.map((turn) => {
    if (!isReply(turn)) return turn;
    const body = replyBody(String(turn["content"]));
    if (body.length < 400 || body.startsWith("(settled")) return turn;
    const first = seen.get(body);
    if (first !== undefined) {
      return {
        ...turn,
        content: wrap(`[identical to the reply in row ${String(first)} — the same ${body.length} bytes, not repeated]`),
      };
    }
    seen.set(body, turn["id"] ?? null);
    return turn;
  });
}

/** How to get a reply's body again, in ONE line: the replying agent's own
 *  `origin` when it wrote one; otherwise the call that produced it — re-sending
 *  it may not give the same bytes, and the line says so. */
export function originLine(result: Json, target: string, verb: string, payload: Payload): string {
  if (result !== null && typeof result === "object" && !Array.isArray(result)) {
    const o = result["origin"];
    if (typeof o === "string" && o.trim() !== "") return o.split(/\s+/).filter(Boolean).join(" ").slice(0, 300);
  }
  const args = Object.fromEntries(
    Object.entries(payload).filter(([k]) => k !== "type" && k !== "client_id" && k !== "_call_stack"),
  );
  return `${target} ${verb} ${JSON.stringify(args).slice(0, 200)} — re-send it; the output may differ`;
}
