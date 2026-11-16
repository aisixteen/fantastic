// THE FILE VIEW'S HEADER, as data: the breadcrumb, the line count and the meta line under the
// name. Pure; no DOM. ONE implementation for both drawings of a file: the canvas previewer
// imports this module, and the file view's html (`file_view.html`) carries it spliced in
// (`views.rs` drops each `export ` keyword, nothing else), so the two cannot drift.

/** The extension of a file name, lower case; "" for none or a dotfile. */
export function extOf(name) {
  const i = name.lastIndexOf(".");
  return i <= 0 ? "" : name.slice(i + 1).toLowerCase();
}

/** `1.5 KiB`, `2.0 MiB`; `unknown size` for a size the bridge could not give. */
export function human(n) {
  if (n < 0) return "unknown size";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1024 / 1024).toFixed(1)} MiB`;
}

/** The breadcrumb of a path: its segments, the project root first when the path is empty. */
export function crumbs(path) {
  if (path === null) return [];
  if (path === "") return ["project"];
  return path.split("/").filter((s) => s !== "");
}

/** `612 bytes`, then the larger units of `human`. */
export function sizeText(n) {
  if (n < 0) return "unknown size";
  if (n < 1024) return `${n} ${n === 1 ? "byte" : "bytes"}`;
  return human(n);
}

/** The line count of a text. A trailing newline ends the last line, it does not start one. */
export function lineCount(text) {
  if (text === "") return 0;
  const n = text.split("\n").length;
  return text.endsWith("\n") ? n - 1 : n;
}

/** `32 lines`, `1 line`. */
export function linesText(n) {
  return `${n} ${n === 1 ? "line" : "lines"}`;
}

/** `32 lines · 612 bytes · yaml`: the lines only when known (`lines`, or counted from `text`), the size and the extension when known. */
export function metaOf(o) {
  const parts = [];
  const lines = o.lines !== undefined ? o.lines : o.text !== undefined ? lineCount(o.text) : undefined;
  if (lines !== undefined) parts.push(linesText(lines));
  if (o.size !== undefined && o.size >= 0) parts.push(sizeText(o.size));
  const ext = extOf(o.name);
  if (ext !== "") parts.push(ext);
  return parts.join(" · ");
}

/** The agents above (and including) agent `id`, outermost first, as their shown names: its `parentId` chain as far as `index` knows it (`index`: agentAt(path), folderOf(id)). */
export function agentChain(id, index) {
  const out = [];
  const seen = new Set();
  let cur = id;
  while (cur !== undefined && !seen.has(cur)) {
    seen.add(cur);
    const folder = index.folderOf(cur);
    const info = folder === undefined ? undefined : index.agentAt(folder);
    if (info === undefined) {
      if (cur === id) out.push(id);
      break;
    }
    out.push(info.displayName !== "" ? info.displayName : info.id);
    cur = info.parentId;
  }
  return out.reverse();
}

/** The breadcrumb of a path inside an agent's folder: the agent chain, then the path below that folder. Outside any agent folder it is `crumbs`. `index` also gives paths(): the agent folders it knows. */
export function trail(path, index) {
  if (path === null || path === "") return crumbs(path);
  let home = "";
  for (const p of index.paths()) {
    if ((path === p || path.startsWith(`${p}/`)) && p.length > home.length) home = p;
  }
  const info = home === "" ? undefined : index.agentAt(home);
  if (info === undefined) return crumbs(path);
  return [...agentChain(info.id, index), ...crumbs(path.slice(home.length))];
}
