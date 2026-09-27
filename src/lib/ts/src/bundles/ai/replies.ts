import type { Json, Payload } from "../../kernel/json.ts";
import type { Turn } from "./context.ts";

// REPLIES — the tag every reply is known by, and the view trimmers that read it.
// Port of python `ai_core/core.py` (`_reply_tag`, `_REPLY_TAG`, `_reply_blocks`,
// `drop_answered_reflects`, `dedupe_stale_reads`, `dedupe_repeat_bodies`,
// `_origin_line`, `_clean_title`) and `strategies/strip.py`'s `_stub`.
//
// A delivered turn carries ONE OR MORE reply blocks — a batch arrives whole, one
// block per call, joined by a blank line — each opening with its tag:
//   [reply to <pid> · <target> <verb> · "<title>" · ↑<message> · N of M in batch <b>]

/** `[^\]]*` absorbs the optional segments. The verb group is `[^\]\s]+`, NOT
 *  `\S+`: with `\S+` a body carrying a `]` (every reflect body with a list) let
 *  the tag close on the BODY's bracket and the verb read `reflect]`. */
export const REPLY_TAG = /^\[reply to \S+ · (\S+) ([^\]\s]+)[^\]]*\]/;
const BLOCK_SPLIT = /\n\n(?=\[reply to \S+ · )/;
/** The pid off a block's first line — what `grep` and `strip` address a body by. */
export const PID_TAG = /^\[reply to (\S+) · /;

export function replyBlocks(content: string): string[] {
  return content.split(BLOCK_SPLIT);
}

/** The one line a reply is known by, forever — in the store, in recall pages,
 *  and as the stub a stripped body leaves behind. Optional segments in order. */
export function replyTag(
  pid: Json,
  target: string,
  verb: string,
  title: string | null,
  messageId: Json,
  n: number,
  mTotal: number,
  batch: string,
): string {
  let tag = `[reply to ${String(pid)} · ${target} ${verb}`;
  if (title) tag += ` · "${title}"`;
  if (messageId) tag += ` · ↑${String(messageId)}`;
  if (mTotal > 1) tag += ` · ${n} of ${mTotal} in batch ${batch}`;
  return tag + "]";
}

/** A title lives INSIDE the reply tag, so it may not contain the character that
 *  closes it, nor a newline that would end the tag line. Capped: it is a sentence. */
export function cleanTitle(t: unknown): string | null {
  if (typeof t !== "string" || t.trim() === "") return null;
  return t.replace(/[\]\r\n]+/g, " ").trim().slice(0, 140) || null;
}

/** A pid token as the STORE keys it: an int when it is one, the raw token otherwise. */
export function pidOf(token: string): number | string {
  return /^\d+$/.test(token) ? Number(token) : token;
}

export function isReply(m: Turn): boolean {
  const c = m["content"];
  return m["role"] === "user" && typeof c === "string" && c.startsWith("[reply to ");
}

/** The stub a stripped block leaves: its tag line, and how big the body was. */
export function stubBlock(block: string): string {
  const nl = block.indexOf("\n");
  const head = nl === -1 ? block : block.slice(0, nl);
  const body = nl === -1 ? "" : block.slice(nl + 1);
  return `${head} (stripped: ${body.length} bytes — recall by pid)`;
}

/** The resolution a rendered row is at: a fold, a stripped stub, or the row itself. */
export function resOf(row: Turn): "fold" | "stub" | "full" {
  if (row["kind"] === "fold") return "fold";
  const c = row["content"];
  if (typeof c === "string" && c.includes("(stripped:") && c.startsWith("[reply to ")) return "stub";
  return "full";
}

/** Render a stored row at the resolution the view array asks for. */
export function applyRes(row: Turn, res: string): Turn {
  const c = row["content"];
  if (res === "stub" && typeof c === "string" && c.startsWith("[reply to ")) {
    return { ...row, content: replyBlocks(c).map(stubBlock).join("\n\n") };
  }
  return row;
}

// ─── the reflect distillation ────────────────────────────────────────────

/** A reflect that the model has ALREADY ANSWERED is replaced by this stub. */
const REFLECT_DROPPED = (agent: string): string => `[reflect of ${agent} — full text dropped; your distillation follows]`;
/** What counts as "the model recorded it": a `SEEN <agent> — …` line. */
const DISTILLED = /\bSEEN\b/i;

export function distilled(text: string): boolean {
  return DISTILLED.test(text);
}

/** Rides ON a reflect reply (point of use, not policy), deliberately LAST: it is
 *  what the model reads immediately before writing its answer. */
export const REFLECT_NUDGE =
  "\n[this body is TRANSIENT — after your next message it is replaced by a stub. " +
  "Open that message with the facts you will need, one compact line per agent:\n" +
  "SEEN <id> — <what it is for>; verbs: <the ones you intend to use>. " +
  "One reflect per agent is complete — reflecting it again yields nothing new; " +
  "the next useful call is an action.]";

/**
 * Replace the body of every reflect reply the model has already answered. FAIL
 * CLOSED: only when the answering turn actually wrote the distillation (`SEEN`).
 * The IN-FLIGHT reflect (not yet answered) stays verbatim. Per BLOCK: only the
 * reflect bodies drop; sibling replies of the batch stay. Row keys ride through.
 */
export function dropAnsweredReflects(history: Turn[]): Turn[] {
  return history.map((turn, i) => {
    const content = String(turn["content"] ?? "");
    const m = turn["role"] === "user" ? REPLY_TAG.exec(content) : null;
    const answer = history.slice(i + 1).find((t) => t["role"] === "assistant");
    const ok = answer !== undefined && distilled(String(answer["content"] ?? ""));
    if (m === null || !ok) return turn;
    const blocks = replyBlocks(content);
    const trimmed = blocks.map((b) => {
      const mb = REPLY_TAG.exec(b);
      return mb !== null && mb[2] === "reflect" ? REFLECT_DROPPED(mb[1] as string) : b;
    });
    return trimmed.every((b, j) => b === blocks[j]) ? turn : { ...turn, content: trimmed.join("\n\n") };
  });
}

/** Verbs that return WHOLE state: the newest reply IS the truth. Parameterised
 *  reads (`read`, `list`, `status`, `get`) are never deduped — two of them are
 *  different answers and the tag carries no args to tell them apart. */
export const WHOLE_STATE_READS: ReadonlySet<string> = new Set(["reflect", "list_agents", "ps", "keys"]);

/** Drop whole-state read replies that a later reply to the same (agent, verb)
 *  supersedes. Per BLOCK: a superseded block drops alone; a turn whose every
 *  block is superseded drops entirely. Newest wins; order otherwise preserved. */
export function dedupeStaleReads(history: Turn[]): Turn[] {
  const seen = new Set<string>();
  const keepRev: Turn[] = [];
  for (let i = history.length - 1; i >= 0; i--) {
    const turn = history[i] as Turn;
    const content = String(turn["content"] ?? "");
    if (turn["role"] !== "user" || !REPLY_TAG.test(content)) {
      keepRev.push(turn);
      continue;
    }
    const keptRev: string[] = [];
    let changed = false;
    for (const b of replyBlocks(content).reverse()) {
      const mb = REPLY_TAG.exec(b);
      if (mb !== null && WHOLE_STATE_READS.has(mb[2] as string)) {
        const key = `${mb[1]}\u0000${mb[2]}`;
        if (seen.has(key)) {
          changed = true;
          continue; // a later reply already carries this state
        }
        seen.add(key);
      }
      keptRev.push(b);
    }
    if (keptRev.length === 0) continue;
    keepRev.push(changed ? { ...turn, content: keptRev.reverse().join("\n\n") } : turn);
  }
  return keepRev.reverse();
}

/** A reply whose BODY is byte-identical to an EARLIER one is shown as a one-line
 *  stub citing the first — the first copy stays whole. Only bodies of 400+
 *  chars; a digest (its origin line) is already one line. The view restarts at a
 *  clear, so a read after a clear is new again. */
export function dedupeRepeatBodies(history: Turn[]): Turn[] {
  const seen = new Map<string, Json>();
  return history.map((turn) => {
    const content = String(turn["content"] ?? "");
    if (turn["role"] !== "user" || !REPLY_TAG.test(content)) return turn;
    let changed = false;
    const kept = replyBlocks(content).map((b) => {
      const nl = b.indexOf("\n");
      const head = nl === -1 ? b : b.slice(0, nl);
      const body = nl === -1 ? "" : b.slice(nl + 1);
      if (body.length < 400 || body.startsWith("(settled")) return b;
      const first = seen.get(body);
      if (first !== undefined) {
        changed = true;
        return `${head}\n[identical to the reply in row ${String(first)} — the same ${body.length} bytes, not repeated]`;
      }
      seen.set(body, (turn["id"] as Json | undefined) ?? null);
      return b;
    });
    return changed ? { ...turn, content: kept.join("\n\n") } : turn;
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
  return `${target} ${verb} ${pyJson(args).slice(0, 200)} — re-send it; the output may differ`;
}

/** python `json.dumps(..., ensure_ascii=False)` spacing for the origin line. */
function pyJson(v: unknown): string {
  if (v instanceof Uint8Array) return `"<bytes:${v.length}>"`;
  if (v === null || v === undefined) return "null";
  if (Array.isArray(v)) return `[${v.map(pyJson).join(", ")}]`;
  if (typeof v === "object") return `{${Object.entries(v).map(([k, x]) => `${JSON.stringify(k)}: ${pyJson(x)}`).join(", ")}}`;
  return JSON.stringify(v);
}
