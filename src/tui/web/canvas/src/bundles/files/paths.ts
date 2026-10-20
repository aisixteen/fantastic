// PROJECT PATHS, as the file tree names them: root-relative, `/`-separated, the
// root itself `""` (the file bridge says `"."`). Pure; no DOM.

/** Folders the tree hides until "show ignored" is on. */
export const IGNORED: readonly string[] = [".git", "node_modules", "target", "dist", "__pycache__"];

/** The project's own agents folder: highlighted in the tree. */
export const FANTASTIC = ".fantastic";

/** A path as the tree keys it: no `./`, no leading or trailing `/`; the root is `""`. */
export function norm(p: string): string {
  let s = p.replace(/\\/g, "/");
  while (s.startsWith("./")) s = s.slice(2);
  s = s.replace(/\/{2,}/g, "/").replace(/^\/+|\/+$/g, "");
  return s === "." ? "" : s;
}

/** The path a bridge call carries: the root is `.`. */
export const wire = (p: string): string => (p === "" ? "." : p);

export function parentOf(p: string): string {
  const i = p.lastIndexOf("/");
  return i < 0 ? "" : p.slice(0, i);
}

export function nameOf(p: string): string {
  return p.slice(p.lastIndexOf("/") + 1);
}

export const join = (dir: string, name: string): string => (dir === "" ? name : `${dir}/${name}`);

/** Every folder above `p`, root first: `a/b/c` → `["", "a", "a/b"]`. The root has none. */
export function ancestors(p: string): string[] {
  if (p === "") return [];
  const out = [""];
  const parts = p.split("/");
  for (let i = 1; i < parts.length; i++) out.push(parts.slice(0, i).join("/"));
  return out;
}

/** Whether `p` stays inside the project: not absolute, no `..` segment, no drive letter. */
export function inRoot(p: string): boolean {
  if (p.startsWith("/") || p.startsWith("\\") || /^[A-Za-z]:/.test(p)) return false;
  return !p.split(/[\\/]/).includes("..");
}

export const ignored = (name: string): boolean => IGNORED.includes(name);

export const isFantastic = (p: string): boolean => p === FANTASTIC || p.startsWith(`${FANTASTIC}/`);

/** Whether `p` is `prefix` or below it. */
export const under = (p: string, prefix: string): boolean => prefix === "" || p === prefix || p.startsWith(`${prefix}/`);
