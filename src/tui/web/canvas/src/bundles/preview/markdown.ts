import { norm, parentOf } from "../files/paths.ts";

// MARKDOWN for the previewer: a small parser to a block AST, and a renderer that
// builds nodes with `createElement` and `textContent` ONLY — file text never
// reaches `innerHTML`. Not CommonMark: headings, paragraphs, bullet and ordered
// lists (nested by indent), fenced code, quotes, rules, simple tables, and
// emphasis, strong, code, links and images inline.

export type Inline =
  | { t: "text"; v: string }
  | { t: "em" | "strong"; c: Inline[] }
  | { t: "code"; v: string }
  | { t: "link"; href: string; c: Inline[] }
  | { t: "image"; src: string; alt: string };

export type Block =
  | { t: "heading"; level: number; c: Inline[] }
  | { t: "para"; c: Inline[] }
  | { t: "list"; ordered: boolean; items: Block[][] }
  | { t: "code"; lang: string; v: string }
  | { t: "quote"; c: Block[] }
  | { t: "hr" }
  | { t: "table"; head: Inline[][]; rows: Inline[][][] };

const SCHEME = /^[a-z][a-z0-9+.-]*:/i;

/** A link target the previewer will follow: `http(s):`, `mailto:`, or a relative path. Anything else (`javascript:`, `data:`, `file:`, `//host`) is dropped. */
export function linkKind(href: string): "web" | "mail" | "file" | "anchor" | null {
  const h = href.trim();
  if (h === "") return null;
  if (/^https?:\/\//i.test(h)) return "web";
  if (/^mailto:/i.test(h)) return "mail";
  if (h.startsWith("#")) return "anchor";
  if (h.startsWith("//") || h.startsWith("/") || h.startsWith("\\") || SCHEME.test(h)) return null;
  return "file";
}

/** A relative `href` from the markdown file in folder `dir`, as a project path; null when it leaves the project. */
export function resolveRel(dir: string, href: string): string | null {
  const bare = href.split(/[?#]/)[0]!;
  // no path at all (a query or an anchor alone) is not the folder: nothing to open
  if (bare.split("/").every((seg) => seg === "" || seg === ".")) return null;
  const parts = (dir === "" ? [] : dir.split("/")).slice();
  for (const seg of bare.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") {
      if (parts.length === 0) return null;
      parts.pop();
    } else parts.push(seg);
  }
  const p = norm(parts.join("/"));
  return p === "" ? null : p;
}

// ─── inline ──────────────────────────────────────────────────────────────

export function parseInline(s: string): Inline[] {
  const out: Inline[] = [];
  let text = "";
  const flush = (): void => {
    if (text !== "") out.push({ t: "text", v: text });
    text = "";
  };
  let i = 0;
  while (i < s.length) {
    const c = s[i]!;
    if (c === "\\" && i + 1 < s.length && /[\\`*_{}[\]()#+\-.!|>]/.test(s[i + 1]!)) {
      text += s[i + 1];
      i += 2;
      continue;
    }
    if (c === "`") {
      const end = s.indexOf("`", i + 1);
      if (end > i) {
        flush();
        out.push({ t: "code", v: s.slice(i + 1, end) });
        i = end + 1;
        continue;
      }
    }
    if (c === "!" && s[i + 1] === "[") {
      const m = /^!\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/.exec(s.slice(i));
      if (m !== null) {
        flush();
        out.push({ t: "image", src: m[2]!, alt: m[1]! });
        i += m[0].length;
        continue;
      }
    }
    if (c === "[") {
      const m = /^\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/.exec(s.slice(i));
      if (m !== null) {
        flush();
        out.push({ t: "link", href: m[2]!, c: parseInline(m[1]!) });
        i += m[0].length;
        continue;
      }
    }
    if ((c === "*" || c === "_") && s[i + 1] === c) {
      const end = s.indexOf(c + c, i + 2);
      if (end > i + 2) {
        flush();
        out.push({ t: "strong", c: parseInline(s.slice(i + 2, end)) });
        i = end + 2;
        continue;
      }
    }
    if ((c === "*" || c === "_") && s[i + 1] !== " " && s[i + 1] !== undefined && (c === "*" || i === 0 || /\W/.test(s[i - 1]!))) {
      const end = s.indexOf(c, i + 1);
      if (end > i + 1 && s[end - 1] !== " " && (c === "*" || end + 1 >= s.length || /\W/.test(s[end + 1]!))) {
        flush();
        out.push({ t: "em", c: parseInline(s.slice(i + 1, end)) });
        i = end + 1;
        continue;
      }
    }
    text += c;
    i++;
  }
  flush();
  return out;
}

// ─── blocks ──────────────────────────────────────────────────────────────

const FENCE = /^\s{0,3}(```+|~~~+)\s*([\w+-]*)/;
const HEADING = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const HR = /^\s{0,3}([-*_])(?:\s*\1){2,}\s*$/;
const ITEM = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
const TABLE_SEP = /^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)*\|?\s*$/;

const indentOf = (l: string): number => l.length - l.trimStart().length;
const cells = (l: string): string[] => l.trim().replace(/^\|/, "").replace(/\|$/, "").split("|").map((c) => c.trim());

export function parseMarkdown(src: string): Block[] {
  return blocks(src.replace(/\r\n?/g, "\n").split("\n"));
}

function blocks(lines: string[]): Block[] {
  const out: Block[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i]!;
    if (line.trim() === "") {
      i++;
      continue;
    }
    const f = FENCE.exec(line);
    if (f !== null) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i]!.trimStart().startsWith(f[1]!)) body.push(lines[i++]!);
      i++; // the closing fence (or past the end: an unterminated fence runs to the end)
      out.push({ t: "code", lang: f[2]!, v: body.join("\n") });
      continue;
    }
    const h = HEADING.exec(line);
    if (h !== null) {
      out.push({ t: "heading", level: h[1]!.length, c: parseInline(h[2]!) });
      i++;
      continue;
    }
    if (HR.test(line)) {
      out.push({ t: "hr" });
      i++;
      continue;
    }
    if (/^\s{0,3}>/.test(line)) {
      const q: string[] = [];
      while (i < lines.length && /^\s{0,3}>/.test(lines[i]!)) q.push(lines[i++]!.replace(/^\s{0,3}>\s?/, ""));
      out.push({ t: "quote", c: blocks(q) });
      continue;
    }
    if (ITEM.test(line)) {
      const r = list(lines, i);
      out.push(r.block);
      i = r.next;
      continue;
    }
    if (line.includes("|") && i + 1 < lines.length && TABLE_SEP.test(lines[i + 1]!) && lines[i + 1]!.includes("-")) {
      const head = cells(line).map(parseInline);
      const rows: Inline[][][] = [];
      i += 2;
      while (i < lines.length && lines[i]!.trim() !== "" && lines[i]!.includes("|")) rows.push(cells(lines[i++]!).map(parseInline));
      out.push({ t: "table", head, rows });
      continue;
    }
    const para: string[] = [];
    while (i < lines.length && lines[i]!.trim() !== "" && !FENCE.test(lines[i]!) && !HEADING.test(lines[i]!) && !HR.test(lines[i]!) && !/^\s{0,3}>/.test(lines[i]!) && !(para.length > 0 && ITEM.test(lines[i]!))) {
      para.push(lines[i++]!.trim());
    }
    out.push({ t: "para", c: parseInline(para.join(" ")) });
  }
  return out;
}

/** A list starting at `at`: items at its indent; deeper lines belong to the item above. */
function list(lines: string[], at: number): { block: Block; next: number } {
  const first = ITEM.exec(lines[at]!)!;
  const base = first[1]!.length;
  const ordered = /\d/.test(first[2]!);
  const items: Block[][] = [];
  let i = at;
  while (i < lines.length) {
    const m = ITEM.exec(lines[i]!);
    if (m === null || m[1]!.length !== base || /\d/.test(m[2]!) !== ordered) break;
    const body: string[] = [m[3]!];
    i++;
    // the item's own continuation: deeper-indented lines (blank lines inside them included)
    while (i < lines.length) {
      const l = lines[i]!;
      if (l.trim() === "") {
        const nxt = lines.slice(i + 1).find((x) => x.trim() !== "");
        if (nxt !== undefined && indentOf(nxt) > base) {
          body.push("");
          i++;
          continue;
        }
        break;
      }
      if (indentOf(l) > base) {
        body.push(l.slice(Math.min(indentOf(l), base + 2)));
        i++;
        continue;
      }
      if (ITEM.test(l) || FENCE.test(l) || HEADING.test(l)) break;
      body.push(l.trim()); // a lazy continuation of the item's paragraph
      i++;
    }
    items.push(blocks(body));
    while (i < lines.length && lines[i]!.trim() === "" && ITEM.test(lines[i + 1] ?? "") && indentOf(lines[i + 1]!) === base) i++;
  }
  return { block: { t: "list", ordered, items }, next: i };
}

// ─── render ──────────────────────────────────────────────────────────────

export interface RenderHooks {
  /** the folder of the markdown file: relative links and images resolve from it */
  dir: string;
  /** a project image: set `img.src` (it is read through the bridge) */
  image(img: HTMLImageElement, path: string): void;
  /** a relative link to a project file */
  open(path: string): void;
}

export function renderMarkdown(ast: Block[], doc: Document, hooks: RenderHooks): HTMLElement {
  const root = doc.createElement("div");
  root.className = "pv-md";
  const el = (tag: string, parent: Node, cls?: string): HTMLElement => {
    const e = doc.createElement(tag);
    if (cls !== undefined) e.className = cls;
    parent.appendChild(e);
    return e;
  };
  const inline = (xs: Inline[], parent: Node): void => {
    for (const x of xs) {
      switch (x.t) {
        case "text":
          parent.appendChild(doc.createTextNode(x.v));
          break;
        case "em":
        case "strong":
          inline(x.c, el(x.t, parent));
          break;
        case "code":
          el("code", parent).textContent = x.v;
          break;
        case "image": {
          const kind = linkKind(x.src);
          const rel = kind === "file" ? resolveRel(hooks.dir, x.src) : null;
          if (rel !== null) {
            const img = el("img", parent) as HTMLImageElement;
            img.alt = x.alt;
            hooks.image(img, rel);
          } else {
            // a remote or unsafe image is never fetched for the file's author: a link instead
            const span = el("span", parent, "pv-md-noimg");
            span.textContent = `[image: ${x.alt || x.src}]`;
          }
          break;
        }
        case "link": {
          const kind = linkKind(x.href);
          if (kind === null || kind === "anchor") {
            inline(x.c, el("span", parent)); // a link the previewer will not follow: its text
          } else if (kind === "file") {
            const rel = resolveRel(hooks.dir, x.href);
            const a = el("a", parent);
            a.dataset["path"] = rel ?? "";
            a.title = rel ?? x.href;
            a.addEventListener("click", (e) => {
              e.preventDefault();
              if (rel !== null) hooks.open(rel);
            });
            inline(x.c, a);
          } else {
            const a = el("a", parent) as HTMLAnchorElement;
            a.href = x.href;
            a.target = "_blank";
            a.rel = "noopener noreferrer";
            inline(x.c, a);
          }
          break;
        }
      }
    }
  };
  const block = (xs: Block[], parent: Node): void => {
    for (const b of xs) {
      switch (b.t) {
        case "heading":
          inline(b.c, el(`h${b.level}`, parent));
          break;
        case "para":
          inline(b.c, el("p", parent));
          break;
        case "hr":
          el("hr", parent);
          break;
        case "code": {
          const pre = el("pre", parent);
          const code = el("code", pre);
          if (b.lang !== "") code.dataset["lang"] = b.lang;
          code.textContent = b.v;
          break;
        }
        case "quote":
          block(b.c, el("blockquote", parent));
          break;
        case "list": {
          const l = el(b.ordered ? "ol" : "ul", parent);
          for (const item of b.items) block(item, el("li", l));
          break;
        }
        case "table": {
          const t = el("table", parent);
          const tr = el("tr", el("thead", t));
          for (const c of b.head) inline(c, el("th", tr));
          const body = el("tbody", t);
          for (const r of b.rows) {
            const row = el("tr", body);
            for (const c of r) inline(c, el("td", row));
          }
          break;
        }
      }
    }
  };
  block(ast, root);
  return root;
}

/** The folder of a project file path (for `RenderHooks.dir`). */
export const dirOf = (path: string): string => parentOf(path);
