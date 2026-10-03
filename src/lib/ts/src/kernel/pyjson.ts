// Python's TEXT FORMS, for every place this kernel must produce the bytes the
// python kernel produces (python is canonical; rust carries the same helpers):
//
//   pyRepr      — `repr(x)`: how a refusal quotes a caller's value (`'x'`, `None`,
//                 `('frozen', 'live')`), so an error reads the same in every runtime.
//   pyDumps     — `json.dumps(x)` with python's default separators (`", "`,
//                 `": "`) and, by default, ASCII escaping — a wire frame and a
//                 settle digest are byte-identical across runtimes.
//   pySplitlines — `str.splitlines()`: every line boundary python knows.
//   strictLoads — the wire's STRICT JSON (owner D13, 2026-10-01): only values every
//                 runtime can hold. It keeps one fact JS drops: which integral
//                 numbers python reads as a FLOAT (`wireFloat`).

import type { Json } from "./json.ts";

/** Python truthiness — how the python kernel reads a flag (`delete_lock`, a
 *  reply's `error`, a `mode`): null, false, 0, "", [] and {} are false;
 *  everything else is true. */
export function pyTruthy(v: unknown): boolean {
  if (v === undefined || v === null || v === false || v === 0 || v === "") return false;
  if (Array.isArray(v)) return v.length > 0;
  if (typeof v === "object") return Object.keys(v).length > 0;
  return true;
}

/** python `str.isprintable()` is false for these (categories C* and Z*, the
 *  plain space excepted) — `repr` escapes them. */
const NON_PRINTABLE = /[\p{C}\p{Z}]/u;

function reprStr(s: string): string {
  const quote = s.includes("'") && !s.includes('"') ? '"' : "'";
  let out = quote;
  for (const ch of s) {
    const cp = ch.codePointAt(0) as number;
    if (ch === "\\") out += "\\\\";
    else if (ch === quote) out += `\\${quote}`;
    else if (ch === "\n") out += "\\n";
    else if (ch === "\r") out += "\\r";
    else if (ch === "\t") out += "\\t";
    else if (ch !== " " && NON_PRINTABLE.test(ch)) {
      if (cp < 0x100) out += `\\x${cp.toString(16).padStart(2, "0")}`;
      else if (cp < 0x10000) out += `\\u${cp.toString(16).padStart(4, "0")}`;
      else out += `\\U${cp.toString(16).padStart(8, "0")}`;
    } else out += ch;
  }
  return out + quote;
}

/** python `repr(float)` (and so `json.dumps` of a float): the shortest digits that
 *  round-trip — JS and python pick the same ones — in EXPONENT form exactly when
 *  the decimal exponent is below -4 or at least 16 (`1e-05`, `1.5e+16`; JS
 *  switches only below -6 / at 21), with a sign and at least two exponent digits.
 *  An integral JS number reads as a python int (`String(n)`) unless `float` says
 *  python holds it as a float (`1.0`, `1e+20`, `-0.0` — `wireFloat`); what the
 *  wire does with integers beyond 2^53 is an open owner decision, left as it is. */
function reprNum(n: number, float = false): string {
  if (Number.isNaN(n)) return "nan";
  if (n === Infinity) return "inf";
  if (n === -Infinity) return "-inf";
  const integral = Number.isInteger(n);
  if (integral && !float) return String(n);
  if (Object.is(n, -0)) return "-0.0";
  const [mant, exp] = n.toExponential().split("e") as [string, string];
  const e = Number(exp);
  if (e < -4 || e >= 16) return `${mant}e${e < 0 ? "-" : "+"}${String(Math.abs(e)).padStart(2, "0")}`;
  return integral ? `${String(n)}.0` : String(n);
}

/** python `repr(x)` of a JSON value. A JS array reads as a python list; pass
 *  `tuple: true` for a python tuple (`('a', 'b')`). A number off the wire that
 *  python reads as a float reads as one (`wireFloat`). */
export function pyRepr(x: unknown, opts: { tuple?: boolean } = {}): string {
  if (x === null || x === undefined) return "None";
  if (x === true) return "True";
  if (x === false) return "False";
  if (typeof x === "string") return reprStr(x);
  if (typeof x === "number") return reprNum(x);
  if (Array.isArray(x)) {
    const items = x.map((_, i) => pyReprAt(x, i));
    if (opts.tuple === true) return items.length === 1 ? `(${items[0]},)` : `(${items.join(", ")})`;
    return `[${items.join(", ")}]`;
  }
  if (typeof x === "object") {
    return `{${Object.entries(x as { [k: string]: unknown })
      .filter(([, v]) => v !== undefined)
      .map(([k]) => `${reprStr(k)}: ${pyReprAt(x, k)}`)
      .join(", ")}}`;
  }
  return reprStr(String(x));
}

/** python `repr(holder[key])` — a slot of a container, so a number off the wire
 *  that python reads as a float reads as one (`wireFloat`). */
export function pyReprAt(holder: object, key: string | number): string {
  const v = (holder as { [k: string]: unknown })[key];
  return typeof v === "number" && wireFloat(holder, key) ? reprNum(v, true) : pyRepr(v);
}

/** python `type(holder[key]).__name__` — how a refusal names a value it must not
 *  echo (`int`, `NoneType`, `bytes`). A number off the wire that python reads as a
 *  float is a `float` (`wireFloat`); a binary frame's blob is `bytes`. */
export function pyTypeNameAt(holder: object, key: string | number): string {
  const v = (holder as { [k: string]: unknown })[key];
  if (v === null || v === undefined) return "NoneType";
  if (typeof v === "boolean") return "bool";
  if (typeof v === "number") return Number.isInteger(v) && !wireFloat(holder, key) ? "int" : "float";
  if (typeof v === "string") return "str";
  if (v instanceof Uint8Array) return "bytes";
  if (Array.isArray(v)) return "list";
  return "dict";
}

/** python `ensure_ascii` escapes every code unit outside printable ASCII
 *  (`[^\ -~]`): DEL (U+007F) too, and each half of a pair outside the BMP. */
function dumpsStr(s: string, ensureAscii: boolean): string {
  const j = JSON.stringify(s);
  return ensureAscii
    ? j.replace(/[\u007f-\uffff]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`)
    : j;
}

/** python `json.dumps(o)` — `", "` / `": "` separators, `ensure_ascii` on unless
 *  asked off. A key whose value is `undefined` is absent (python has no such
 *  value); a non-JSON leaf is written as `str()` would (python's `default=str`). */
export function pyDumps(o: unknown, opts: { ensureAscii?: boolean } = {}): string {
  const ascii = opts.ensureAscii !== false;
  const walk = (v: unknown): string => {
    if (v === null || v === undefined) return "null";
    if (v === true) return "true";
    if (v === false) return "false";
    if (typeof v === "string") return dumpsStr(v, ascii);
    if (typeof v === "number") {
      if (Number.isNaN(v)) return "NaN";
      if (v === Infinity) return "Infinity";
      if (v === -Infinity) return "-Infinity";
      return reprNum(v);
    }
    if (Array.isArray(v)) return `[${v.map(walk).join(", ")}]`;
    if (typeof v === "object") {
      return `{${Object.entries(v as { [k: string]: unknown })
        .filter(([, x]) => x !== undefined)
        .map(([k, x]) => `${dumpsStr(k, ascii)}: ${walk(x)}`)
        .join(", ")}}`;
    }
    return dumpsStr(String(v), ascii);
  };
  return walk(o);
}

/** python `str.splitlines()`: `\r\n` and every single-character boundary python
 *  knows; a trailing boundary ends the last line rather than opening an empty one. */
export function pySplitlines(s: string): string[] {
  if (s === "") return [];
  const lines = s.split(/\r\n|[\n\r\v\f\x1c\x1d\x1e\x85\u2028\u2029]/);
  if (/(?:\r\n|[\n\r\v\f\x1c\x1d\x1e\x85\u2028\u2029])$/.test(s)) lines.pop();
  return lines;
}

const INT64_MIN = -(2n ** 63n);
const INT64_MAX = 2n ** 63n - 1n;

/** Every number token of a JSON text (outside strings), checked as python's
 *  strict `loads` checks it: an integer must fit int64, any number a double. */
function checkNumbers(text: string): void {
  let inStr = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i] as string;
    if (inStr) {
      if (c === "\\") i++;
      else if (c === '"') inStr = false;
      continue;
    }
    if (c === '"') {
      inStr = true;
      continue;
    }
    if (c === "-" || (c >= "0" && c <= "9")) {
      let j = i + 1;
      while (j < text.length && /[0-9.eE+-]/.test(text[j] as string)) j++;
      const tok = text.slice(i, j);
      if (/^-?\d+$/.test(tok)) {
        const v = BigInt(tok);
        if (v < INT64_MIN || v > INT64_MAX) throw new SyntaxError(`integer ${tok} is outside int64`);
      } else if (!Number.isFinite(Number(tok))) {
        throw new SyntaxError(`number ${tok} overflows a double`);
      }
      i = j - 1;
    }
  }
}

function hasLoneSurrogate(s: string): boolean {
  for (let i = 0; i < s.length; i++) {
    const u = s.charCodeAt(i);
    if (u >= 0xd800 && u <= 0xdbff) {
      const next = s.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        i++;
        continue;
      }
      return true;
    }
    if (u >= 0xdc00 && u <= 0xdfff) return true;
  }
  return false;
}

function checkSurrogates(x: unknown): void {
  if (typeof x === "string") {
    if (hasLoneSurrogate(x)) throw new SyntaxError("a lone surrogate (\\ud800–\\udfff) is not text");
  } else if (Array.isArray(x)) {
    for (const v of x) checkSurrogates(v);
  } else if (x !== null && typeof x === "object") {
    for (const [k, v] of Object.entries(x)) {
      checkSurrogates(k);
      checkSurrogates(v);
    }
  }
}

/** The slots of wire-parsed containers whose number python reads as a FLOAT
 *  while JS holds it as an integer: spelled with a fraction or an exponent
 *  (`1.0`, `1e0`, `1e20`, `-0.0`) — python's `json.loads` makes a float of exactly
 *  those. Keyed by the container the parse built, so the fact lives as long as
 *  that object and is gone from any copy of it. */
const FLOAT_SLOTS = new WeakMap<object, Set<string>>();

/** Whether `holder[key]` came off the wire as a number python reads as a float
 *  though JS holds it as an integer (`strictLoads`). A step there is not a list
 *  index: python refuses `1.0` where it takes `1`. */
export function wireFloat(holder: object, key: string | number): boolean {
  return FLOAT_SLOTS.get(holder)?.has(String(key)) === true;
}

/** The `JSON.parse` reviver of `strictLoads`: reads each number's SPELLING from
 *  the parse's source text (the reviver's `context.source`) and records a slot
 *  whose integral value python reads as a float. A runtime whose `JSON.parse`
 *  gives no source text cannot tell `1.0` from `1`: refused, loud. */
function markFloats(this: object, key: string, value: unknown, context?: { source?: string }): unknown {
  if (typeof value !== "number") return value;
  const source = context?.source;
  if (source === undefined) {
    throw new SyntaxError("JSON.parse here gives no source text (reviver context.source); the wire needs it");
  }
  if (Number.isInteger(value) && /[.eE]/.test(source)) {
    let slots = FLOAT_SLOTS.get(this);
    if (slots === undefined) FLOAT_SLOTS.set(this, (slots = new Set()));
    slots.add(key);
  }
  return value;
}

/** python `io_bridge._codec.loads`: JSON off the wire, STRICTLY. `NaN` /
 *  `Infinity` are not JSON (`JSON.parse` already refuses them); an integer
 *  outside int64, a number that overflows a double and a lone surrogate are
 *  refused too — rust and swift cannot hold them. Throws a `SyntaxError`. An
 *  integral number python reads as a float is recorded (`wireFloat`). */
export function strictLoads(text: string): Json {
  const out = JSON.parse(text, markFloats) as Json; // not JSON at all: refused here
  checkNumbers(text);
  checkSurrogates(out);
  return out;
}
