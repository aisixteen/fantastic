import type { AgentInfo } from "../files/agent_index.ts";

// WHAT THE PREVIEWER SHOWS for a path, and where it is bound. Pure; no DOM.

export type Mode = "dir" | "image" | "pdf" | "markdown" | "text" | "large" | "binary" | "error";

export const IMAGE_MAX = 16 * 1024 * 1024;
export const PDF_MAX = 32 * 1024 * 1024;
/** A text file above this is paged, not edited. */
export const TEXT_MAX = 1024 * 1024;
/** Text above this is shown plain (no highlighting), with a note. */
export const HIGHLIGHT_MAX = 256 * 1024;
/** The `read_stream` page of a large file. */
export const PAGE = 65536;

const IMAGES = ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "avif"];
const BINARIES = ["zip", "gz", "db", "sqlite", "wasm", "exe", "so", "dylib", "dll", "o", "a", "class", "jar", "woff", "woff2", "ttf", "mp3", "mp4", "mov"];

export function extOf(name: string): string {
  const i = name.lastIndexOf(".");
  return i <= 0 ? "" : name.slice(i + 1).toLowerCase();
}

export interface Subject {
  type: "dir" | "file";
  name: string;
  size?: number;
}

/** The mode for a path, from what the listing said of it. */
export function modeFor(s: Subject): Mode {
  if (s.type === "dir") return "dir";
  const ext = extOf(s.name);
  const size = s.size !== undefined && s.size >= 0 ? s.size : 0; // -1: the bridge could not stat it
  if (IMAGES.includes(ext)) return size <= IMAGE_MAX ? "image" : "binary";
  if (ext === "pdf") return size <= PDF_MAX ? "pdf" : "binary";
  if (BINARIES.includes(ext)) return "binary";
  if (size > TEXT_MAX) return "large";
  if (ext === "md" || ext === "markdown") return "markdown";
  return "text";
}

/** A reply that refuses: carries an `error`. */
export const refused = (r: unknown): r is Record<string, unknown> =>
  r !== null && typeof r === "object" && typeof (r as Record<string, unknown>)["error"] === "string";

/** The mode once `read` has answered: the bridge refuses a binary or non-utf-8
 *  file as `bad_arg` on `path` (python and rust alike) → binary; any other
 *  refusal is an error, said verbatim. */
export function refine(mode: Mode, reply: unknown): { mode: Mode; error?: string } {
  if (!refused(reply)) return { mode };
  if ((mode === "text" || mode === "markdown") && reply["reason"] === "bad_arg" && reply["arg"] === "path") return { mode: "binary" };
  return { mode: "error", error: String(reply["error"]) };
}

export const editable = (mode: Mode): boolean => mode === "text" || mode === "markdown";

/** Why a mode cannot be edited ("" when it can). */
export function why(mode: Mode, agent = false): string {
  if (agent) return "agent mode shows the live agent; switch to file mode to edit its files";
  switch (mode) {
    case "text":
    case "markdown":
      return "";
    case "binary":
      return "a binary file is not editable";
    case "large":
      return "a file larger than 1 MiB is paged, not editable";
    case "image":
      return "an image is not editable";
    case "pdf":
      return "a PDF is not editable";
    case "dir":
      return "a folder is not editable";
    case "error":
      return "the file could not be read";
  }
}

/** Where a preview looks: the tree's selection (`follow`) or one path (`pinned`). */
export type Binding = { follow: true } | { pinned: string };

export function targetOf(b: Binding, selection: string | null): string | null {
  return "pinned" in b ? b.pinned : selection;
}

/** What `path` shows in `mode`: an agent folder's file mode is its `agent.yaml`, its agent mode the agent. */
export function agentTarget(path: string, mode: "file" | "agent", index: { agentAt(p: string): AgentInfo | undefined }): { path: string; agent: string | null } {
  const a = index.agentAt(path);
  if (a === undefined) return { path, agent: null };
  return mode === "agent" ? { path, agent: a.id } : { path: `${path}/agent.yaml`, agent: a.id };
}

/** The text of one page of a large file, or null when the page is not text (a NUL
 *  byte, or not UTF-8). `read_stream` is asked for `PAGE + 3` bytes at the page's
 *  offset: the page drops the continuation bytes that finish the previous page's
 *  last character, and ends after the last character it starts, so no character
 *  is split between pages. */
export function pageText(raw: Uint8Array, first: boolean): string | null {
  let from = 0;
  if (!first) while (from < 3 && from < raw.length && (raw[from]! & 0xc0) === 0x80) from++;
  let to = Math.min(PAGE, raw.length);
  while (to < raw.length && (raw[to]! & 0xc0) === 0x80) to++;
  const b = raw.subarray(from, to);
  if (b.includes(0)) return null;
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(b);
  } catch {
    return null;
  }
}

export function human(n: number): string {
  if (n < 0) return "unknown size";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1024 / 1024).toFixed(1)} MiB`;
}
