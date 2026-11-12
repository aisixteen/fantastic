// THE PAGE'S ONE ENGINE for declared verbs. A page agent states its verbs as DATA
// (`Declared`): each verb's arguments with their type, whether they are required, an
// enum, a minimum, a default, and the groups of arguments that go together or that
// are alternatives. `entry` wraps the agent's handler: a call is judged against the
// declaration BEFORE the handler runs, so a handler only ever sees checked,
// completed arguments and has no argument checks of its own. `docsOf` renders the
// page kernel's `docs` from the same declarations (nothing is written twice).
//
// NO coercion: a value of the wrong type is refused, never converted.
import type { Handler, Json, Kernel, Payload } from "../lib.ts";

/** `bytes` is a body of text sent verbatim: a string, or the raw bytes of one (what a binary frame carries). */
export type ArgType = "string" | "number" | "boolean" | "object" | "string[]" | "bytes";

/** One argument of a verb. */
export interface Arg {
  readonly type: ArgType;
  /** What it is for (rendered by `docs`). */
  readonly doc: string;
  readonly required?: true;
  /** The only values a string argument takes. */
  readonly enum?: readonly string[];
  /** A number's least value; a string's least length; a list's least length. */
  readonly min?: number;
  /** Put in when the call leaves the argument out (shown by `docs`). */
  readonly default?: Json;
  /** An object argument carries only these keys. */
  readonly keys?: readonly string[];
}

/** One verb: what it does and the arguments it takes. */
export interface Verb {
  readonly doc: string;
  readonly args: { readonly [name: string]: Arg };
  /** Alternatives: the call names at least one of these groups (every argument of a group). */
  readonly anyOf?: readonly (readonly string[])[];
  /** Alternatives: the call names exactly one of these arguments. */
  readonly oneOf?: readonly string[];
  /** Arguments given together or not at all. */
  readonly together?: readonly (readonly string[])[];
  /** When `arg` has `is`, `then` is required as well. */
  readonly when?: readonly { readonly arg: string; readonly is: string; readonly then: string }[];
  /** Arguments beyond the declared ones are the handler's to report (not refused). */
  readonly open?: true;
}

export type Declared = { readonly [verb: string]: Verb };

/** The one refusal builder of the page's verbs. */
export function refuse(reason: string, error: string, extra: Record<string, unknown> = {}): Json {
  return { error, reason, ...extra } as Json;
}

const typeName = (v: unknown): string => (v === null ? "null" : Array.isArray(v) ? "list" : typeof v);

function typed(a: Arg, v: unknown): boolean {
  switch (a.type) {
    case "string": return typeof v === "string" && (a.enum === undefined || a.enum.includes(v)) && (a.min === undefined || v.length >= a.min);
    case "number": return typeof v === "number" && Number.isFinite(v) && (a.min === undefined || v >= a.min);
    case "bytes": return typeof v === "string" || v instanceof ArrayBuffer || ArrayBuffer.isView(v);
    case "boolean": return typeof v === "boolean";
    case "object": return typeof v === "object" && v !== null && !Array.isArray(v);
    case "string[]": return Array.isArray(v) && v.every((x) => typeof x === "string") && (a.min === undefined || v.length >= a.min);
  }
}

const wanted = (a: Arg): string =>
  a.enum !== undefined ? `one of ${a.enum.join(", ")}`
  : a.type === "number" && a.min !== undefined ? `a number of at least ${a.min}`
  : a.type === "string" && a.min !== undefined ? `a string of at least ${a.min} characters`
  : a.type === "string[]" ? `a list of strings${a.min !== undefined ? ` (at least ${a.min})` : ""}`
  : a.type === "bytes" ? "text (verbatim)"
  : a.type === "object" ? "an object"
  : `a ${a.type}`;

/** The arguments of `payload` checked against the verb `name` and completed with its defaults, or
 *  the refusal. */
export function check(owner: string, name: string, v: Verb, payload: Payload): { args: Payload } | { refusal: Json } {
  const args: Payload = { ...payload };
  const given = (k: string): boolean => args[k] !== undefined && args[k] !== null;
  for (const [k, a] of Object.entries(v.args)) {
    if (!given(k)) {
      if (a.required) return { refusal: refuse("missing_arg", `${owner}: ${name} needs ${k}`, { arg: k, verb: name }) };
      if (a.default !== undefined) args[k] = a.default;
      continue;
    }
    if (!typed(a, args[k])) {
      return { refusal: refuse("bad_arg", `${owner}: ${name} — ${k} must be ${wanted(a)}, got ${typeName(args[k])}`, { arg: k, verb: name }) };
    }
    if (a.keys !== undefined) {
      const stray = Object.keys(args[k] as object).filter((x) => !a.keys!.includes(x));
      if (stray.length > 0) {
        return { refusal: refuse("bad_arg", `${owner}: ${name} — ${k} takes ${a.keys.join(", ")}; not ${stray.join(", ")}`, { arg: k, verb: name }) };
      }
    }
  }
  if (v.open !== true) {
    const stray = Object.keys(args).filter((k) => k !== "type" && v.args[k] === undefined);
    if (stray.length > 0) {
      return { refusal: refuse("bad_arg", `${owner}: ${name} takes ${Object.keys(v.args).join(", ") || "no arguments"}; not ${stray[0]}`, { arg: stray[0]!, verb: name }) };
    }
  }
  if (v.anyOf !== undefined && !v.anyOf.some((g) => g.every(given))) {
    const names = v.anyOf.map((g) => g.join("+")).join(" or ");
    return { refusal: refuse("missing_arg", `${owner}: ${name} needs ${names}`, { arg: v.anyOf[0]![0]!, verb: name }) };
  }
  if (v.oneOf !== undefined) {
    const named = v.oneOf.filter(given);
    if (named.length === 0) return { refusal: refuse("missing_arg", `${owner}: ${name} needs one of ${v.oneOf.join(", ")}`, { arg: v.oneOf[0]!, verb: name }) };
    if (named.length > 1) return { refusal: refuse("bad_arg", `${owner}: ${name} takes one of ${v.oneOf.join(", ")}, not ${named.join(" and ")}`, { arg: named[1]!, verb: name }) };
  }
  for (const g of v.together ?? []) {
    const have = g.filter(given);
    if (have.length > 0 && have.length < g.length) {
      const lacks = g.find((k) => !given(k))!;
      return { refusal: refuse("bad_arg", `${owner}: ${name} — ${g.join(" and ")} go together; ${lacks} is missing`, { arg: lacks, verb: name }) };
    }
  }
  for (const w of v.when ?? []) {
    if (args[w.arg] === w.is && !given(w.then)) {
      return { refusal: refuse("missing_arg", `${owner}: ${name} ${w.is} needs ${w.then}`, { arg: w.then, verb: name }) };
    }
  }
  return { args };
}

/** The handler behind the declaration: a call is resolved and checked before `handler` runs. A verb
 *  that is not declared but is named in `other` (what the page kernel's lifecycle sends) goes to
 *  `handler` as it is; any other is refused, naming the verbs the agent answers. */
export function entry(owner: string, declared: Declared, handler: Handler, other: readonly string[] = []): Handler {
  return (id, payload, kernel: Kernel): Json | Promise<Json> => {
    const name = typeof payload["type"] === "string" ? payload["type"] : "";
    if (other.includes(name)) return handler(id, payload, kernel);
    const v = declared[name];
    if (v === undefined) {
      return refuse("unhandled", `${owner}: unknown verb '${name}' — it answers ${Object.keys(declared).join(", ")}`, { verb: name });
    }
    const checked = check(owner, name, v, payload);
    if ("refusal" in checked) return checked.refusal;
    return handler(id, checked.args, kernel);
  };
}

const sig = (k: string, a: Arg): string =>
  `${k}:${a.enum !== undefined ? a.enum.join("|") : a.type}${a.required ? "" : "?"}${a.default !== undefined ? ` = ${JSON.stringify(a.default)}` : ""}`;

/** The text `docs` serves for one verb, rendered from its declaration. */
export function docOf(v: Verb): string {
  const args = Object.entries(v.args);
  const list = args.length === 0 ? "No args." : `args: ${args.map(([k, a]) => sig(k, a)).join(", ")}.`;
  const each = args.filter(([, a]) => a.doc !== "").map(([k, a]) => `${k}: ${a.doc}`).join(" ");
  return [v.doc, list, each].filter((s) => s !== "").join(" ");
}

/** The page kernel's per-verb docs (`registerBundle`'s table) rendered from the declarations. */
export function docsOf(declared: Declared): { [verb: string]: string } {
  return Object.fromEntries(Object.entries(declared).map(([n, v]) => [n, docOf(v)]));
}
