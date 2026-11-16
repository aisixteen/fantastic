import type { Mode } from "./preview_model.ts";

// THE FILE VIEW'S HEADER, as data: the breadcrumb, the kind chip and the meta
// line under the name. The breadcrumb and meta logic is the file bridge's own
// (`fantastic-file/src/file_head.mjs`), shared with the html file view; the kind chip
// and the no-view note are the previewer's.

export { agentChain, crumbs, lineCount, metaOf, sizeText, trail } from "../../../../../../lib/rust/crates/bundles/fantastic-file/src/file_head.mjs";
export type { AgentLookup } from "../../../../../../lib/rust/crates/bundles/fantastic-file/src/file_head.mjs";

/** The kind chip: what the path is on disk. */
export function kindOf(mode: Mode, agent: boolean): "file" | "folder" | "agent" {
  if (agent) return "agent";
  return mode === "dir" ? "folder" : "file";
}

/** The handler a "no view for X" refusal names: an agent that has no view of its own. */
export function noViewHandler(message: string): string | null {
  const m = /^no view for (.+)$/.exec(message);
  return m === null ? null : m[1]!;
}
