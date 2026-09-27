import { callSpans } from "./tool_parse.ts";

// Thinking: PARSED, not PERSISTED. Port of python `ai_core/core.py`
// `without_thinking`. A provider wraps a reasoning model's thought in
// `<think>…</think>`; the FULL text is parsed for calls (a call written mid-thought
// still dispatches) and the STRIPPED text is what history keeps.

export const THINK_OPEN = "<think>";
export const THINK_CLOSE = "</think>";

/** Every COMPLETE call envelope inside `span`, in source order, one per line —
 *  an action taken inside a thought is not reasoning. */
function keptEnvelopes(span: string): string {
  const found = callSpans(span);
  return found.length === 0 ? "" : "\n" + found.join("\n") + "\n";
}

/** Drop every `<think>…</think>` span — what goes into durable history — KEEPING
 *  any call envelope found inside a dropped span, since dispatch parses the full
 *  text: the reasoning goes, the action stays. An unterminated thought (a stream
 *  cut mid-thought) drops its tail but keeps a call already written in it. */
export function withoutThinking(text: string): string {
  const out: string[] = [];
  let i = 0;
  for (;;) {
    const a = text.indexOf(THINK_OPEN, i);
    if (a === -1) {
      out.push(text.slice(i));
      break;
    }
    out.push(text.slice(i, a));
    const b = text.indexOf(THINK_CLOSE, a);
    if (b === -1) {
      out.push(keptEnvelopes(text.slice(a)));
      break;
    }
    out.push(keptEnvelopes(text.slice(a, b)));
    i = b + THINK_CLOSE.length;
  }
  return out.join("").trim();
}
