// Thinking: PARSED, not PERSISTED. A provider wraps a reasoning model's thought
// in `<think>…</think>`; the FULL text is parsed for calls, the STRIPPED text is
// what history keeps and a side call (a summary) returns.

export const THINK_OPEN = "<think>";
export const THINK_CLOSE = "</think>";

/** Strip every thought span before a turn is shown or persisted. */
export function withoutThinking(text: string): string {
  return text.replace(/<think>[\s\S]*?<\/think>/g, "").trim();
}
