// A SMALL SYNTAX LEXER for the previewer and the editor — a regex scan, not a
// parser: it colours keywords, strings, comments, numbers and types well enough
// to read code. Linear, never backtracking across lines: a string left open runs
// to the end of its line, a block comment to the end of the file. Invariant: the
// tokens joined back are exactly the source. Pure.

export type TokKind = "kw" | "str" | "com" | "num" | "type" | "punct" | "plain";
export interface Token {
  k: TokKind;
  t: string;
}

export type Lang = "ts" | "json" | "py" | "rs" | "sh" | "yaml" | "css" | "html" | "md" | "plain";

const EXT: Record<string, Lang> = {
  ts: "ts", tsx: "ts", js: "ts", jsx: "ts", mjs: "ts", cjs: "ts", json: "json", jsonc: "json",
  py: "py", rs: "rs", sh: "sh", bash: "sh", zsh: "sh",
  yaml: "yaml", yml: "yaml", toml: "yaml", css: "css", html: "html", htm: "html", xml: "html", svg: "html",
  md: "md", markdown: "md",
};

export function langOf(name: string): Lang {
  const i = name.lastIndexOf(".");
  if (i <= 0) return "plain";
  return EXT[name.slice(i + 1).toLowerCase()] ?? "plain";
}

const words = (s: string): Set<string> => new Set(s.split(" "));
const KEYWORDS: Record<Lang, Set<string>> = {
  ts: words("const let var function return if else for while do switch case break continue new class extends implements interface type enum import export from as default async await try catch finally throw typeof instanceof in of void null undefined true false this super static readonly public private protected declare namespace yield delete"),
  json: words("true false null"),
  py: words("def class return if elif else for while in not and or is import from as with try except finally raise pass break continue lambda yield None True False async await global nonlocal del assert"),
  rs: words("fn let mut const static struct enum impl trait pub use mod crate self super match if else for while loop break continue return as in where move ref async await dyn type unsafe true false Self"),
  sh: words("if then else elif fi for while do done case esac function in return export local echo cd exit set unset source"),
  yaml: words("true false null yes no on off"),
  css: words("important"),
  html: words(""),
  md: words(""),
  plain: words(""),
};

/** Comment openers per language: line comment marker, and a block pair when it has one. */
const LINE: Record<Lang, string | null> = { ts: "//", json: null, py: "#", rs: "//", sh: "#", yaml: "#", css: null, html: null, md: null, plain: null };
const BLOCK: Record<Lang, [string, string] | null> = { ts: ["/*", "*/"], json: null, py: null, rs: ["/*", "*/"], sh: null, yaml: null, css: ["/*", "*/"], html: ["<!--", "-->"], md: null, plain: null };

const IDENT = /[A-Za-z_$][\w$]*/y;
const NUMBER = /(?:0[xX][0-9a-fA-F_]+|\d[\d_]*(?:\.\d+)?(?:[eE][+-]?\d+)?)/y;
const SPACE = /\s+/y;

function at(re: RegExp, s: string, i: number): string | null {
  re.lastIndex = i;
  const m = re.exec(s);
  return m === null ? null : m[0];
}

export function tokens(src: string, lang: Lang): Token[] {
  if (lang === "plain") return src === "" ? [] : [{ k: "plain", t: src }];
  const out: Token[] = [];
  const push = (k: TokKind, t: string): void => {
    if (t === "") return;
    const last = out[out.length - 1];
    if (last !== undefined && last.k === k) last.t += t;
    else out.push({ k, t });
  };
  const kw = KEYWORDS[lang];
  const line = LINE[lang];
  const block = BLOCK[lang];
  const n = src.length;
  let i = 0;
  while (i < n) {
    const c = src[i]!;
    // comments
    if (block !== null && src.startsWith(block[0], i)) {
      const end = src.indexOf(block[1], i + block[0].length);
      const stop = end < 0 ? n : end + block[1].length;
      push("com", src.slice(i, stop));
      i = stop;
      continue;
    }
    if (line !== null && src.startsWith(line, i) && (lang !== "yaml" && lang !== "sh" ? true : i === 0 || /\s/.test(src[i - 1]!))) {
      let stop = src.indexOf("\n", i);
      if (stop < 0) stop = n;
      push("com", src.slice(i, stop));
      i = stop;
      continue;
    }
    // python's triple-quoted strings come first: the single-quote rule below would read `""` as an empty string
    if (lang === "py" && (src.startsWith('"""', i) || src.startsWith("'''", i))) {
      const q = src.slice(i, i + 3);
      const end = src.indexOf(q, i + 3);
      const stop = end < 0 ? n : end + 3;
      push("str", src.slice(i, stop));
      i = stop;
      continue;
    }
    // strings: to the closing quote on the same line, else to the end of the line
    // (prose has apostrophes: markdown takes none, html only double quotes)
    const quoted = lang === "md" ? false : lang === "html" ? c === '"' : c === '"' || c === "'" || (c === "`" && (lang === "ts" || lang === "sh"));
    if (quoted) {
      let j = i + 1;
      if (c === "`" && lang === "ts") {
        // a template literal may span lines
        while (j < n && src[j] !== "`") j += src[j] === "\\" ? 2 : 1;
        j = Math.min(n, j + 1);
      } else {
        while (j < n && src[j] !== c && src[j] !== "\n") j += src[j] === "\\" && j + 1 < n && src[j + 1] !== "\n" ? 2 : 1;
        if (j < n && src[j] === c) j++;
      }
      push("str", src.slice(i, j));
      i = j;
      continue;
    }
    const sp = at(SPACE, src, i);
    if (sp !== null) {
      push("plain", sp);
      i += sp.length;
      continue;
    }
    if (/\d/.test(c)) {
      const num = at(NUMBER, src, i)!;
      push("num", num);
      i += num.length;
      continue;
    }
    const id = at(IDENT, src, i);
    if (id !== null) {
      if (lang === "yaml" && /^\s*[-]?\s*$/.test(src.slice(src.lastIndexOf("\n", i - 1) + 1, i)) && src[i + id.length] === ":") push("type", id);
      else if (kw.has(id)) push("kw", id);
      else if (lang === "css" || lang === "html" || lang === "md") push("plain", id);
      else if (/^[A-Z]/.test(id)) push("type", id);
      else push("plain", id);
      i += id.length;
      continue;
    }
    push(/[{}()[\];,.:<>=+\-*/%!&|^~?@#]/.test(c) ? "punct" : "plain", c);
    i++;
  }
  return out;
}
