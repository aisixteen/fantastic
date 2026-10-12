// HTML VALIDITY — what a panel body must be before it is written (owner,
// 2026-10-06: an AI placed `...html...` as a cell's body and its panel had no
// buttons). Not a full parser — browsers forgive almost anything — but the
// shapes that never render a panel are refused, saying why:
//   · empty, or no element at all (text, a placeholder);
//   · a <script> / <style> / <textarea> opened and never closed (truncated);
//   · ending inside a tag (`<div cla` — truncated).
const RAW = ["script", "style", "textarea"] as const;

/** Why `html` is not a panel body, or null when it is one. */
export function htmlProblem(html: string): string | null {
  const text = html.replace(/<!--[\s\S]*?-->/g, "");
  if (text.trim() === "") return "it is empty";
  if (!/<[a-zA-Z][\w-]*[\s>/]/.test(text) && !/<[a-zA-Z][\w-]*$/.test(text.trim())) {
    const shown = text.trim().slice(0, 40);
    return `it has no element — it reads as text (${JSON.stringify(shown)})`;
  }
  for (const tag of RAW) {
    const open = (text.match(new RegExp(`<${tag}\\b`, "gi")) ?? []).length;
    const close = (text.match(new RegExp(`</${tag}\\s*>`, "gi")) ?? []).length;
    if (open > close) return `a <${tag}> is opened and never closed`;
  }
  // a `<` after the last `>` outside raw text: the markup stops inside a tag
  const tail = text.slice(text.lastIndexOf(">") + 1);
  if (/<[a-zA-Z/!]/.test(tail)) return "it ends inside a tag";
  return null;
}

/** The refusal for a body that is not html, or null. */
export function htmlRefusal(who: string, html: string): Record<string, unknown> | null {
  const why = htmlProblem(html);
  if (why === null) return null;
  return {
    error: `${who}: the html body is not html — ${why}`,
    reason: "bad_arg",
    arg: "bytes",
    hint: "send the panel's WHOLE markup as <bytes>…</bytes> — verbatim, nothing elided",
  };
}
