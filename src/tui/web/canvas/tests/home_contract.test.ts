import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ACTIONS, PROTOCOL, STATUSES, VERSION, decodeState, label } from "../src/bundles/home_view/home_model.ts";
import type { Status } from "../src/bundles/home_view/home_model.ts";
import { HOME_VERBS } from "../src/bundles/home_view/home_view.ts";
import { SCHEMA_BY_PATH, cssName } from "../src/bundles/skin/schema.ts";
import { stockSkin } from "../src/bundles/skin/stock.ts";
import { compileSkin } from "../src/bundles/skin/tokens.ts";

// THE CONTRACT REPLAY, client side: `contract/home/v1/cases.json` (the `home`
// protocol v1, as data) against what the Home reads. The Rust agent replays the
// same file (`fantastic-home-kernel/src/home/contract_tests.rs`).

interface Corpus {
  protocol: string;
  version: number;
  verbs: string[];
  events: string[];
  statuses: string[];
  actions: string[];
  states: { valid: unknown[]; invalid: Array<{ state: unknown; why: string }> };
}

const corpus = JSON.parse(readFileSync(new URL("../../../../../contract/home/v1/cases.json", import.meta.url), "utf8")) as Corpus;

/** The verbs only a host shell sends (`focus` raises its window, `menu` builds
 *  its native menus) and `reflect`, which the page does not send: the Home's
 *  grant is every other corpus verb, plus the stream's `watch`. */
const NOT_THE_PAGES = ["reflect", "focus", "menu"];

test("the corpus is the protocol the model reads", () => {
  assert.equal(corpus.protocol, PROTOCOL);
  assert.equal(corpus.version, VERSION);
  assert.deepEqual(corpus.statuses, [...STATUSES]);
  assert.deepEqual(corpus.actions, [...ACTIONS]);
  assert.ok(corpus.events.includes("state"), "the state is an event");
});

test("HOME_VERBS is the corpus's verbs a page sends, plus watch", () => {
  const want = [...corpus.verbs.filter((v) => !NOT_THE_PAGES.includes(v)), "watch"];
  assert.deepEqual([...HOME_VERBS].sort(), want.sort());
});

test("every corpus status has a label", () => {
  for (const s of corpus.statuses) {
    const text = label(s as Status);
    assert.equal(typeof text, "string", s);
    assert.ok(text.length > 0, s);
  }
});

test("decodeState accepts every valid state and refuses every invalid one, naming why", () => {
  assert.ok(corpus.states.valid.length > 0);
  for (const s of corpus.states.valid) {
    const got = decodeState(s);
    assert.notEqual(typeof got, "string", `refused a valid state: ${String(got)}`);
  }
  assert.ok(corpus.states.invalid.length > 0);
  for (const { state, why } of corpus.states.invalid) {
    const got = decodeState(state);
    assert.equal(typeof got, "string", `accepted an invalid state (${why})`);
    assert.ok((got as string).includes(why), `the refusal ${JSON.stringify(got)} does not name ${why}`);
  }
});

// ── the example client, list.html ───────────────────────────────────────────
const LIST = readFileSync(new URL("../../home/list.html", import.meta.url), "utf8");

test("list.html takes its look from one :root block of the dark skin's custom properties", () => {
  const style = /<style>([\s\S]*?)<\/style>/.exec(LIST)?.[1] ?? "";
  const roots = style.match(/:root\s*\{[^}]*\}/g) ?? [];
  assert.equal(roots.length, 1, "one :root block");
  const dark = compileSkin(stockSkin("dark")!.tokens);
  assert.ok(dark.ok);
  const byName = new Map([...dark.skin.css].map(([path, css]) => [cssName(path), { path, css }]));
  const decls = [...roots[0]!.matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)];
  assert.ok(decls.length > 0);
  for (const [, name, value] of decls) {
    const token = byName.get(name!);
    assert.ok(token !== undefined && SCHEMA_BY_PATH.has(token.path), `${name} is not a schema property`);
    assert.equal(value!.trim(), token.css, `${name} is the dark skin's`);
  }
  // outside that block, every value is a var(--fc-…) of a property it defines
  const rest = style.replace(roots[0]!, "");
  assert.doesNotMatch(rest, /#[0-9a-f]{3,8}\b|rgba?\(|hsla?\(|color-mix|system-ui|monospace/i, "a literal colour outside :root");
  const defined = new Set(decls.map((d) => d[1]));
  for (const [, used] of rest.matchAll(/var\((--[\w-]+)\)/g)) assert.ok(defined.has(used!), `${used} is not in the :root block`);
});

test("list.html checks the protocol first and sends version 1 with a fresh id", () => {
  assert.match(LIST, /type: "reflect"/);
  assert.match(LIST, /me\.protocol !== PROTOCOL \|\| me\.version !== VERSION/);
  assert.match(LIST, /version: VERSION/);
  assert.match(LIST, /crypto\.randomUUID\(\)/);
  assert.match(LIST, /\/home\/ws/);
  assert.match(LIST, /frame\.payload\.type !== "state"/);
});
