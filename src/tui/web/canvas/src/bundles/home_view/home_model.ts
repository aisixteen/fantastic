// THE HOME'S MODEL — pure: no DOM, no kernel. The `home` agent's state as the
// view reads it (protocol `fantastic.home`, version 1, `home/wire.rs`), what each
// status is called and coloured, the card a page shows when its fantastic is not
// running, and which key is which intent. Tested by `tests/home_model.test.ts`.

export const PROTOCOL = "fantastic.home";
export const VERSION = 1;

/** Every status a row can have (`wire.rs` `status_name`). */
export const STATUSES = ["stopped", "starting", "stopping", "running", "no_canvas", "unverified", "missing"] as const;
export type Status = (typeof STATUSES)[number];

/** What a row may be asked, in the agent's order (`Row::actions`). */
export const ACTIONS = ["start", "stop", "open", "remove"] as const;
export type Action = (typeof ACTIONS)[number];

export interface HomeCanvas {
  url: string;
  port: number;
}

export interface HomeRow {
  /** the registry guid, or `"adding:" + path` for a folder being added */
  id: string;
  guid: string | null;
  name: string;
  path: string;
  /** seconds; null for a folder being added */
  last_seen: number | null;
  status: Status;
  /** set while running */
  canvas: HomeCanvas | null;
  pid: number | null;
  actions: Action[];
  /** the launch log file, when it exists */
  log: string | null;
}

export interface HomeState {
  type: "state";
  protocol: typeof PROTOCOL;
  version: typeof VERSION;
  seq: number;
  mode: "lock" | "canvas";
  listed: boolean;
  rows: HomeRow[];
  /** the open pages: row ids, in order */
  pages: string[];
  /** the shown page, or null for the Home */
  selection: string | null;
  problem: string | null;
}

type Rec = Record<string, unknown>;
const isRec = (v: unknown): v is Rec => typeof v === "object" && v !== null && !Array.isArray(v);
const show = (v: unknown): string => (v === undefined ? "missing" : JSON.stringify(v));

/** One row, or the reason it is not one. */
function decodeRow(v: unknown, at: string): HomeRow | string {
  if (!isRec(v)) return `${at} is not an object`;
  const str = (k: string): string | null => (typeof v[k] === "string" ? null : `${at}.${k}: ${show(v[k])} is not a string`);
  const strOrNull = (k: string): string | null =>
    v[k] === null || typeof v[k] === "string" ? null : `${at}.${k}: ${show(v[k])} is not a string or null`;
  const numOrNull = (k: string): string | null =>
    v[k] === null || (typeof v[k] === "number" && Number.isFinite(v[k])) ? null : `${at}.${k}: ${show(v[k])} is not a number or null`;
  const bad = str("id") ?? strOrNull("guid") ?? str("name") ?? str("path") ?? numOrNull("last_seen") ?? numOrNull("pid") ?? strOrNull("log");
  if (bad !== null) return bad;
  if (!(STATUSES as readonly unknown[]).includes(v["status"])) return `${at}.status: ${show(v["status"])} is not a status (${STATUSES.join(", ")})`;
  const c = v["canvas"];
  if (c !== null && !(isRec(c) && typeof c["url"] === "string" && typeof c["port"] === "number")) {
    return `${at}.canvas: ${show(c)} is not {url, port} or null`;
  }
  const a = v["actions"];
  if (!Array.isArray(a)) return `${at}.actions: ${show(a)} is not a list`;
  const odd = a.find((x) => !(ACTIONS as readonly unknown[]).includes(x));
  if (odd !== undefined) return `${at}.actions: ${show(odd)} is not an action (${ACTIONS.join(", ")})`;
  return v as unknown as HomeRow;
}

/** The home's state, or the reason `v` is not one (e.g. ``state: `rows` is not a list``). */
export function decodeState(v: unknown): HomeState | string {
  if (!isRec(v)) return `state: ${show(v)} is not an object`;
  if (v["type"] !== "state") return `state: \`type\` is ${show(v["type"])}, not "state"`;
  if (v["protocol"] !== PROTOCOL) return `state: \`protocol\` is ${show(v["protocol"])}, not "${PROTOCOL}"`;
  if (v["version"] !== VERSION) return `state: \`version\` is ${show(v["version"])}, not ${VERSION}`;
  if (typeof v["seq"] !== "number" || !Number.isInteger(v["seq"])) return `state: \`seq\` is ${show(v["seq"])}, not an integer`;
  if (v["mode"] !== "lock" && v["mode"] !== "canvas") return `state: \`mode\` is ${show(v["mode"])}, not "lock" or "canvas"`;
  if (typeof v["listed"] !== "boolean") return `state: \`listed\` is ${show(v["listed"])}, not a boolean`;
  if (!Array.isArray(v["rows"])) return "state: `rows` is not a list";
  for (const [i, r] of (v["rows"] as unknown[]).entries()) {
    const row = decodeRow(r, `state: rows[${i}]`);
    if (typeof row === "string") return row;
  }
  const pages = v["pages"];
  if (!Array.isArray(pages) || !pages.every((p) => typeof p === "string")) return "state: `pages` is not a list of ids";
  if (v["selection"] !== null && typeof v["selection"] !== "string") return `state: \`selection\` is ${show(v["selection"])}, not an id or null`;
  if (v["problem"] !== null && typeof v["problem"] !== "string") return `state: \`problem\` is ${show(v["problem"])}, not a string or null`;
  return v as unknown as HomeState;
}

const LABELS: Readonly<Record<Status, string>> = {
  running: "Running",
  starting: "Starting",
  stopping: "Stopping",
  no_canvas: "Running without a canvas",
  unverified: "Running, but not answering as this fantastic",
  stopped: "Stopped",
  missing: "Folder not found",
};

/** What a status is called. */
export const label = (status: Status): string => LABELS[status];

/** The custom property that colours a status (`home_css.ts` sets every one). */
export const colourVar = (status: Status): string => `--home-status-${status}`;

const UNITS: ReadonlyArray<[Intl.RelativeTimeFormatUnit, number]> = [
  ["year", 365 * 86400],
  ["month", 30 * 86400],
  ["week", 7 * 86400],
  ["day", 86400],
  ["hour", 3600],
  ["minute", 60],
];
const RELATIVE = new Intl.RelativeTimeFormat("en", { numeric: "auto" });

/** "last seen <relative>" for a `last_seen` in seconds, at `nowMs`. */
export function lastSeen(lastSeenS: number, nowMs: number): string {
  const ago = Math.max(0, nowMs / 1000 - lastSeenS);
  for (const [unit, s] of UNITS) {
    if (ago >= s) return `last seen ${RELATIVE.format(-Math.floor(ago / s), unit)}`;
  }
  return `last seen ${RELATIVE.format(-Math.floor(ago), "second")}`;
}

/** A verb a state card offers. */
export type CardVerb = "start" | "stop" | "remove";

export interface StateCard {
  title: string;
  detail: string;
  verbs: CardVerb[];
}

/** The card a page shows while its fantastic is not running; null while it
 *  runs (the page shows its canvas). */
export function stateCard(row: HomeRow, nowMs: number = Date.now()): StateCard | null {
  const seen = row.last_seen !== null && row.last_seen > 0 ? lastSeen(row.last_seen, nowMs) : null;
  switch (row.status) {
    case "running":
      return null;
    case "starting":
      return { title: `Starting ${row.name}…`, detail: row.path, verbs: [] };
    case "stopping":
      return { title: `Stopping ${row.name}…`, detail: row.path, verbs: [] };
    case "stopped":
      return { title: row.name, detail: seen === null ? "Stopped" : `Stopped · ${seen}`, verbs: ["start"] };
    case "no_canvas":
      return {
        title: "Running without a canvas",
        detail: `${row.name} runs in a terminal or headless. Stop it there or here, then start it here to open its canvas.`,
        verbs: ["stop"],
      };
    case "unverified":
      return {
        title: "Not answering",
        detail: `A fantastic runs in ${row.path}, but it is not the run the list describes, or its canvas does not answer as ${row.name}. Stop it and start it again.`,
        verbs: ["stop"],
      };
    case "missing":
      return { title: "Folder not found", detail: `${row.path} is gone, or no longer holds a fantastic.`, verbs: ["remove"] };
  }
}

/** A state card's buttons: its verbs that the row's actions offer. */
export const cardVerbs = (row: HomeRow, nowMs?: number): CardVerb[] =>
  (stateCard(row, nowMs)?.verbs ?? []).filter((v) => row.actions.includes(v));

/** What a verb's button says (on a state card, `remove` is "Remove from list"). */
export function verbLabel(verb: string, onCard = false): string {
  if (verb === "remove") return onCard ? "Remove from list" : "Remove";
  return verb.charAt(0).toUpperCase() + verb.slice(1);
}

/** An intent: a `home` verb and its arguments. */
export interface Intent {
  verb: "select" | "select_page" | "close_page";
  args: Record<string, unknown>;
}

export interface KeyLike {
  key: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
}

/** The intent a key asks for, or null: mod+0 the Home, mod+1…9 a page in
 *  range, mod+w the shown page. The mod is ⌘ on a Mac and Ctrl elsewhere; the
 *  other one, Alt and Shift must be up. */
export function shortcut(e: KeyLike, isMac: boolean, state: Pick<HomeState, "pages" | "selection">): Intent | null {
  const mod = isMac ? e.metaKey : e.ctrlKey;
  const other = isMac ? e.ctrlKey : e.metaKey;
  if (!mod || other || e.altKey || e.shiftKey) return null;
  if (e.key === "0") return { verb: "select", args: { id: null } };
  if (/^[1-9]$/.test(e.key)) {
    const index = Number(e.key) - 1;
    return index < state.pages.length ? { verb: "select_page", args: { index } } : null;
  }
  if (e.key === "w" || e.key === "W") return state.selection === null ? null : { verb: "close_page", args: { id: state.selection } };
  return null;
}
