import { norm } from "./paths.ts";
import type { DragItem } from "./drag.ts";

// THE BLUEPRINT DROP (#269): a folder holding `blueprint.yaml`, dragged onto a cell or the
// canvas, is imported with the `user` agent's `import_subtree` instead of opened as a file
// tree. The drop first asks for the PREVIEW (no `approve`): nothing is created until the human
// approves it. Pure: no DOM, no kernel; the canvas hands in `send` and the dialog.

type Rec = Record<string, unknown>;

/** `blueprint.yaml` in a folder's `list` reply (the file bridge's `{files: [{path, type}]}`): the folder is a blueprint. */
export function isBlueprintDir(item: DragItem, listing: unknown): boolean {
  if (item.kind !== "dir") return false;
  const files = (listing as Rec | null)?.["files"];
  if (!Array.isArray(files)) return false;
  return files.some((f) => {
    const o = (f ?? {}) as Rec;
    return o["type"] !== "dir" && /(^|\/)blueprint\.yaml$/.test(norm(String(o["path"] ?? "")));
  });
}

export interface ImportOpts {
  bind?: Record<string, string>;
  allowCode?: boolean;
  approve?: boolean;
}

/** The `import_subtree` payload for `user`. `allow_code` and `approve` are the literal `true` or absent. */
export function importCall(path: string, parentId: string, o: ImportOpts = {}): Rec {
  return {
    type: "import_subtree",
    path,
    parent_id: parentId,
    bind: { ...(o.bind ?? {}) },
    ...(o.allowCode === true ? { allow_code: true } : {}),
    ...(o.approve === true ? { approve: true } : {}),
  };
}

/** Names from a list of strings, or a map's values (the verb's reply holds `ids` as `{blueprint id: new id}`). */
const idsOf = (v: unknown): string => {
  const list = Array.isArray(v) ? v : v !== null && typeof v === "object" ? Object.values(v) : [];
  return list.filter((x): x is string => typeof x === "string").join(", ");
};

/** The lines of an `approval_required` reply: the new ids, what each reaches, the ports, the code, the hint. */
export function previewLines(reply: Rec): string[] {
  const out: string[] = [];
  const ids = Object.values((reply["new_ids"] ?? {}) as Rec).map(String);
  out.push(`would create ${String(reply["agents"] ?? ids.length)} agents: ${ids.join(", ")}`);
  for (const r of (Array.isArray(reply["reaches"]) ? reply["reaches"] : []) as Rec[]) {
    const parts: string[] = [];
    if (idsOf(r["can_call"]) !== "") parts.push(`calls ${idsOf(r["can_call"])}`);
    if (idsOf(r["callable_by"]) !== "") parts.push(`called by ${idsOf(r["callable_by"])}`);
    out.push(`${String(r["id"])} (from ${String(r["from"])}): reaches ${parts.length > 0 ? parts.join("; ") : "nothing outside"}`);
  }
  const ports = Object.entries((reply["ports"] ?? {}) as Rec);
  out.push(ports.length > 0 ? `ports: ${ports.map(([p, i]) => `${p} = ${String(i)}`).join(", ")}` : "ports: none");
  const code = Array.isArray(reply["code"]) ? (reply["code"] as unknown[]) : [];
  if (code.length > 0) out.push("code:", ...code.map((c) => `  ${typeof c === "string" ? c : JSON.stringify(c)}`));
  else out.push("no code");
  if (typeof reply["hint"] === "string") out.push(reply["hint"]);
  return out;
}

/** The ports a reply needs bound: the `unbound_ports` refusal names them (`@memory (memory), …: pass bind`). */
export function portsOf(reply: Rec): string[] {
  if (reply["reason"] !== "unbound_ports") return [];
  const head = String(reply["error"] ?? "").split(": pass bind")[0] ?? "";
  return [...head.matchAll(/@[^\s,():]+/g)].map((m) => m[0]);
}

/** The agent a drop imports under: the cell under the pointer (its backend), else the canvas's host node. */
export function dropParent(
  target: { closest(sel: string): { dataset: { id?: string } } | null } | null,
  where: { backendOf(cellId: string): string | undefined; canvasHost: string },
): string {
  const id = target?.closest(".agent-frame")?.dataset.id;
  return (id !== undefined ? where.backendOf(id) : undefined) ?? where.canvasHost;
}

/** What the dialog shows: the lines, the ports to bind, whether code is in it. */
export interface ImportView {
  kind: "bind" | "preview" | "refused" | "done";
  lines: string[];
  ports: string[];
  code: boolean;
}

/** The human's answer: `approve` false is Cancel. `allowCode` is its own tick. */
export interface ImportAnswer {
  approve: boolean;
  bind: Record<string, string>;
  allowCode: boolean;
}

export interface ImportDrop {
  /** the `user` agent's reply to one `import_subtree` payload */
  send(payload: Rec): Promise<Rec>;
  /** shows a view; for `bind` and `preview` the answer decides, for the others it is ignored */
  show(view: ImportView): Promise<ImportAnswer> | ImportAnswer | void;
  item: DragItem;
  parentId: string;
}

const refusalText = (r: Rec): string => `✕ ${String(r["error"])}`;

/**
 * Preview first (no `approve`); show it; send the approving call ONLY when the human approves.
 * Ports the blueprint needs are asked for, then previewed again. A refusal is shown with its
 * message and nothing more is sent. Cancel sends nothing more.
 */
export async function runImportDrop(d: ImportDrop): Promise<void> {
  if (d.item.kind !== "dir") return;
  const path = norm(d.item.path);
  let bind: Record<string, string> = {};
  let reply: Rec = {};
  for (let tries = 0; tries < 3; tries++) {
    reply = await d.send(importCall(path, d.parentId, { bind }));
    const ports = portsOf(reply);
    if (ports.length === 0) break;
    const a = await d.show({ kind: "bind", lines: [String(reply["error"])], ports, code: false });
    if (a === undefined || !a.approve) return;
    bind = { ...bind, ...a.bind };
  }
  if (typeof reply["error"] === "string") {
    await d.show({ kind: "refused", lines: [refusalText(reply)], ports: [], code: false });
    return;
  }
  if (reply["approval_required"] !== true) {
    await d.show({ kind: "refused", lines: [`✕ unexpected reply: ${JSON.stringify(reply)}`], ports: [], code: false });
    return;
  }
  const lines = previewLines(reply);
  const code = Array.isArray(reply["code"]) && (reply["code"] as unknown[]).length > 0;
  const a = await d.show({ kind: "preview", lines, ports: [], code });
  if (a === undefined || !a.approve) return;
  const done = await d.send(importCall(path, d.parentId, { bind, allowCode: code && a.allowCode, approve: true }));
  if (typeof done["error"] === "string") {
    await d.show({ kind: "refused", lines: [refusalText(done)], ports: [], code: false });
    return;
  }
  await d.show({ kind: "done", lines: [`imported ${idsOf(done["ids"])}`], ports: [], code: false });
}
