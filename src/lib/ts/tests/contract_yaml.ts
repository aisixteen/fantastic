// A STRICT YAML subset — exactly what `contract/` is written in, nothing more.
//
// The ts kernel ships zero runtime dependencies and no YAML parser (the browser
// has no use for one), so the contract-corpus runner carries its own, in TEST
// code only. It reads: block mappings and sequences by indentation, a sequence
// item that opens a mapping on its dash line, flow `{…}` / `[…]` (nested, may
// span lines), plain / single-quoted / double-quoted scalars (a plain scalar may
// continue on more-indented lines), folded `>` and literal `|` block scalars, and
// `#` comments. Typing is YAML 1.2 core: true/false, null/~, ints, floats, else
// a string; a quoted scalar is always a string.
//
// Anything else — anchors, aliases, tags, `? ` keys, a tab in indentation, a
// duplicate key, an unterminated quote or bracket — THROWS naming the file and
// line, rather than reading as something the author did not write.

export class YamlError extends Error {}

type Node = unknown;

interface Raw {
  n: number;
  indent: number;
  text: string; // comment stripped, right-trimmed, indentation removed
}

/** Is `i` at the start of a token — where a quote may open a quoted scalar? */
function tokenStart(s: string, i: number): boolean {
  for (let j = i - 1; j >= 0; j--) {
    const c = s[j] as string;
    if (c === " ") continue;
    return ":-[{,".includes(c);
  }
  return true;
}

/** Drop a `#` comment (at line start, or after whitespace, outside quotes). */
function stripComment(s: string): string {
  let q: string | null = null;
  for (let i = 0; i < s.length; i++) {
    const c = s[i] as string;
    if (q !== null) {
      if (q === '"' && c === "\\") i++;
      else if (c === q) {
        if (q === "'" && s[i + 1] === "'") i++;
        else q = null;
      }
      continue;
    }
    if ((c === '"' || c === "'") && tokenStart(s, i)) q = c;
    else if (c === "#" && (i === 0 || s[i - 1] === " ")) return s.slice(0, i);
  }
  return s;
}

function plainScalar(s: string): Node {
  const t = s.trim();
  if (t === "" || t === "~" || t === "null") return null;
  if (t === "true") return true;
  if (t === "false") return false;
  if (/^[-+]?\d+$/.test(t)) return Number.parseInt(t, 10);
  if (/^[-+]?(\d+\.\d*|\.\d+)([eE][-+]?\d+)?$/.test(t)) return Number(t);
  return t;
}

export function parseYaml(text: string, where: string): Node {
  const lines = text.split(/\r?\n/);
  let i = 0;

  const fail = (n: number, why: string): never => {
    throw new YamlError(`${where}:${n + 1}: ${why}`);
  };

  /** The next meaningful line at or after `i` (blank / comment-only skipped). */
  const peek = (): Raw | null => {
    for (let j = i; j < lines.length; j++) {
      const raw = lines[j] as string;
      if (/^\t/.test(raw) || /^ *\t/.test(raw)) fail(j, "a tab in indentation");
      const body = stripComment(raw).replace(/\s+$/, "");
      if (body.trim() === "") continue;
      const indent = body.length - body.trimStart().length;
      return { n: j, indent, text: body.slice(indent) };
    }
    return null;
  };
  const advancePast = (r: Raw): void => {
    i = r.n + 1;
  };

  // ── quoted scalars ──────────────────────────────────────────────────────
  const quoted = (s: string, at: number, n: number): [string, number] => {
    const q = s[at] as string;
    let out = "";
    for (let j = at + 1; j < s.length; j++) {
      const c = s[j] as string;
      if (q === "'") {
        if (c === "'") {
          if (s[j + 1] === "'") {
            out += "'";
            j++;
            continue;
          }
          return [out, j + 1];
        }
        out += c;
      } else {
        if (c === "\\") {
          const e = s[++j];
          const map: { [k: string]: string } = { n: "\n", t: "\t", '"': '"', "\\": "\\", "/": "/", r: "\r", "0": "\0" };
          if (e === undefined || !(e in map)) fail(n, `unsupported escape \\${e ?? ""}`);
          out += map[e as string];
          continue;
        }
        if (c === '"') return [out, j + 1];
        out += c;
      }
    }
    return fail(n, "an unterminated quoted scalar");
  };

  // ── flow collections ────────────────────────────────────────────────────
  const flow = (s: string, n: number): Node => {
    let p = 0;
    const ws = (): void => {
      while (p < s.length && s[p] === " ") p++;
    };
    const value = (stop: string): Node => {
      ws();
      const c = s[p];
      if (c === "{") return map();
      if (c === "[") return seq();
      if (c === '"' || c === "'") {
        const [v, end] = quoted(s, p, n);
        p = end;
        return v;
      }
      const start = p;
      while (p < s.length && !stop.includes(s[p] as string)) p++;
      return plainScalar(s.slice(start, p));
    };
    const map = (): Node => {
      p++; // {
      const out: { [k: string]: Node } = {};
      ws();
      if (s[p] === "}") {
        p++;
        return out;
      }
      for (;;) {
        ws();
        let key: string;
        if (s[p] === '"' || s[p] === "'") {
          const [k, end] = quoted(s, p, n);
          key = k;
          p = end;
        } else {
          const start = p;
          while (p < s.length && s[p] !== ":") p++;
          key = s.slice(start, p).trim();
        }
        ws();
        if (s[p] !== ":") fail(n, `a flow mapping key without ':' near ${JSON.stringify(key)}`);
        p++;
        if (key in out) fail(n, `duplicate key ${JSON.stringify(key)}`);
        out[key] = value(",}");
        ws();
        if (s[p] === ",") {
          p++;
          continue;
        }
        if (s[p] === "}") {
          p++;
          return out;
        }
        return fail(n, "an unterminated flow mapping");
      }
    };
    const seq = (): Node => {
      p++; // [
      const out: Node[] = [];
      ws();
      if (s[p] === "]") {
        p++;
        return out;
      }
      for (;;) {
        out.push(value(",]"));
        ws();
        if (s[p] === ",") {
          p++;
          continue;
        }
        if (s[p] === "]") {
          p++;
          return out;
        }
        return fail(n, "an unterminated flow sequence");
      }
    };
    const v = value("");
    ws();
    if (p !== s.length) fail(n, `trailing text after a flow value: ${JSON.stringify(s.slice(p))}`);
    return v;
  };

  /** Brackets still open in `s` (outside quotes). */
  const depth = (s: string): number => {
    let d = 0;
    let q: string | null = null;
    for (let j = 0; j < s.length; j++) {
      const c = s[j] as string;
      if (q !== null) {
        if (q === '"' && c === "\\") j++;
        else if (c === q) q = null;
        continue;
      }
      if ((c === '"' || c === "'") && tokenStart(s, j)) q = c;
      else if (c === "{" || c === "[") d++;
      else if (c === "}" || c === "]") d--;
    }
    return d;
  };

  /** An inline value on a key/dash line, plus whatever continues it below. */
  const inline = (rest: string, r: Raw, ownerIndent: number): Node => {
    if (/^[&*!]/.test(rest) || rest.startsWith("? ")) fail(r.n, "anchors, aliases, tags and complex keys are not supported");
    if (rest === ">" || rest === "|" || rest === ">-" || rest === "|-") {
      const folded = rest.startsWith(">");
      const body: string[] = [];
      let j = r.n + 1;
      let blockIndent = -1;
      for (; j < lines.length; j++) {
        const raw = lines[j] as string;
        if (raw.trim() === "") {
          body.push("");
          continue;
        }
        const ind = raw.length - raw.trimStart().length;
        if (ind <= ownerIndent) break;
        if (blockIndent < 0) blockIndent = ind;
        body.push(raw.slice(Math.min(ind, blockIndent)));
      }
      i = j;
      while (body.length > 0 && body[body.length - 1] === "") body.pop();
      if (!folded) return body.join("\n") + (rest.endsWith("-") ? "" : "\n");
      const paras: string[] = [];
      let cur: string[] = [];
      for (const b of body) {
        if (b === "") {
          paras.push(cur.join(" "));
          cur = [];
        } else cur.push(b.trim());
      }
      paras.push(cur.join(" "));
      return paras.join("\n") + (rest.endsWith("-") ? "" : "\n");
    }
    advancePast(r);
    if (rest.startsWith("{") || rest.startsWith("[")) {
      let s = rest;
      while (depth(s) > 0) {
        const nx = peek();
        if (nx === null) fail(r.n, "an unterminated flow collection");
        s += " " + (nx as Raw).text;
        advancePast(nx as Raw);
      }
      return flow(s, r.n);
    }
    if (rest.startsWith('"') || rest.startsWith("'")) {
      const [v, end] = quoted(rest, 0, r.n);
      if (rest.slice(end).trim() !== "") fail(r.n, "text after a quoted scalar");
      return v;
    }
    // a PLAIN scalar may continue on more-indented lines, folded with spaces
    let s = rest;
    for (;;) {
      const nx = peek();
      if (nx === null || nx.indent <= ownerIndent) break;
      s += " " + nx.text.trim();
      advancePast(nx);
    }
    return plainScalar(s);
  };

  /** Split `key: rest` at the mapping indicator, outside quotes and brackets. */
  const splitKey = (text: string, n: number): [string, string] | null => {
    let q: string | null = null;
    let d = 0;
    if (text.startsWith('"') || text.startsWith("'")) {
      const [k, end] = quoted(text, 0, n);
      const after = text.slice(end);
      if (after === ":" || after.startsWith(": ")) return [k, after.slice(1).trim()];
      return null;
    }
    for (let j = 0; j < text.length; j++) {
      const c = text[j] as string;
      if (q !== null) {
        if (c === q) q = null;
        continue;
      }
      if ((c === '"' || c === "'") && tokenStart(text, j) && j > 0) q = c;
      else if (c === "{" || c === "[") d++;
      else if (c === "}" || c === "]") d--;
      else if (c === ":" && d === 0 && (j === text.length - 1 || text[j + 1] === " ")) {
        return [text.slice(0, j).trim(), text.slice(j + 1).trim()];
      }
    }
    return null;
  };

  const isSeqLine = (t: string): boolean => t === "-" || t.startsWith("- ");

  const node = (minIndent: number): Node => {
    const r = peek();
    if (r === null || r.indent < minIndent) return null;
    return isSeqLine(r.text) ? sequence(r.indent) : mapping(r.indent);
  };

  const valueAfterKey = (rest: string, r: Raw, ownerIndent: number): Node => {
    if (rest !== "") return inline(rest, r, ownerIndent);
    advancePast(r);
    const nx = peek();
    if (nx === null) return null;
    if (nx.indent > ownerIndent) return node(nx.indent);
    if (nx.indent === ownerIndent && isSeqLine(nx.text)) return sequence(ownerIndent);
    return null;
  };

  const mapping = (ind: number, first?: Raw): Node => {
    const out: { [k: string]: Node } = {};
    let r: Raw | null = first ?? peek();
    while (r !== null && r.indent === ind && !isSeqLine(r.text)) {
      const kv = splitKey(r.text, r.n);
      if (kv === null) fail(r.n, `expected 'key: value', got ${JSON.stringify(r.text)}`);
      const [key, rest] = kv as [string, string];
      if (key in out) fail(r.n, `duplicate key ${JSON.stringify(key)}`);
      out[key] = valueAfterKey(rest, r, ind);
      r = peek();
    }
    if (r !== null && r.indent > ind) fail(r.n, `unexpected indentation (${r.indent} under a mapping at ${ind})`);
    return out;
  };

  const sequence = (ind: number): Node => {
    const out: Node[] = [];
    let r = peek();
    while (r !== null && r.indent === ind && isSeqLine(r.text)) {
      const content = r.text === "-" ? "" : r.text.slice(2);
      const itemIndent = ind + 2 + (content.length - content.trimStart().length);
      const c = content.trim();
      if (c === "") {
        advancePast(r);
        const nx = peek();
        out.push(nx !== null && nx.indent > ind ? node(nx.indent) : null);
      } else if (!c.startsWith("{") && !c.startsWith("[") && splitKey(c, r.n) !== null) {
        // a mapping opened on the dash line: its keys sit at the dash + 2
        out.push(mapping(itemIndent, { n: r.n, indent: itemIndent, text: c }));
      } else {
        out.push(inline(c, r, ind));
      }
      r = peek();
    }
    if (r !== null && r.indent > ind) fail(r.n, `unexpected indentation (${r.indent} under a sequence at ${ind})`);
    return out;
  };

  const first = peek();
  if (first === null) return null;
  const doc = node(first.indent);
  const rest = peek();
  if (rest !== null) fail(rest.n, `unparsed content: ${JSON.stringify(rest.text)}`);
  return doc;
}
