import { test } from "node:test";
import assert from "node:assert/strict";
import { langOf, tokens } from "../src/bundles/preview/highlight.ts";
import type { Lang } from "../src/bundles/preview/highlight.ts";

const SAMPLES: Record<Lang, string> = {
  ts: "const x: Map<string, number> = new Map(); // note\n/* a\nblock */ let s = `t ${1}\nmore`; if (x) return 0x1F;\n",
  json: '{"a": [1, 2.5e3, true, null], "b": "str\\"q"}',
  py: 'def f(a):\n    """doc\n    more"""\n    # c\n    return {"k": None, \'s\': 1}\n',
  rs: 'fn main() { let s: &str = "hi"; /* c */ println!("{}", 1_000); } // end\n',
  sh: 'if [ -f "$X" ]; then echo \'a b\' # c\nfi\n',
  yaml: "id: files\nlist:\n  - a: 'b' # c\n  - 3\n",
  css: ".a { color: red; /* c */ margin: 0 4px; }\n",
  html: '<div class="a">text <!-- c --> it\'s</div>\n',
  md: "# Title\n\nit's *fine* with `code`\n",
  plain: "nothing special\n",
};

test("the tokens joined back are exactly the source, in every language", () => {
  for (const [lang, src] of Object.entries(SAMPLES) as Array<[Lang, string]>) {
    assert.equal(tokens(src, lang).map((t) => t.t).join(""), src, lang);
  }
});

test("empty and odd input still round-trips", () => {
  for (const lang of Object.keys(SAMPLES) as Lang[]) {
    for (const src of ["", "\n", "   ", "\u0000�", "é世界 😀"]) assert.equal(tokens(src, lang).map((t) => t.t).join(""), src, `${lang} ${JSON.stringify(src)}`);
  }
});

test("an unterminated string runs to the end of its line; an unterminated block comment to the end of the file", () => {
  const t = tokens('let a = "open\nlet b = 1', "ts");
  assert.deepEqual(t.filter((x) => x.k === "str").map((x) => x.t), ['"open']);
  assert.ok(t.some((x) => x.k === "num" && x.t === "1"), "the next line lexes normally");
  const c = tokens("a /* open\nstill comment", "ts");
  assert.deepEqual(c.filter((x) => x.k === "com").map((x) => x.t), ["/* open\nstill comment"]);
});

test("kinds: keywords, strings, comments, numbers, types", () => {
  const t = tokens('const N = 12; // c\nlet s = "x"; class Foo {}', "ts");
  const kinds = (k: string): string[] => t.filter((x) => x.k === k).map((x) => x.t);
  assert.deepEqual(kinds("kw"), ["const", "let", "class"]);
  assert.deepEqual(kinds("num"), ["12"]);
  assert.deepEqual(kinds("com"), ["// c"]);
  assert.deepEqual(kinds("str"), ['"x"']);
  assert.ok(kinds("type").includes("Foo"));
});

test("yaml: a key is a type, a trailing # starts a comment only after space", () => {
  const t = tokens("key: value # c\nurl: a#b\n", "yaml");
  assert.deepEqual(t.filter((x) => x.k === "com").map((x) => x.t), ["# c"]);
});

test("langOf by extension", () => {
  assert.equal(langOf("a.ts"), "ts");
  assert.equal(langOf("a.JSON"), "json");
  assert.equal(langOf("x.py"), "py");
  assert.equal(langOf("x.rs"), "rs");
  assert.equal(langOf("x.sh"), "sh");
  assert.equal(langOf("agent.yaml"), "yaml");
  assert.equal(langOf("Cargo.toml"), "yaml");
  assert.equal(langOf("a.css"), "css");
  assert.equal(langOf("a.html"), "html");
  assert.equal(langOf("README.md"), "md");
  assert.equal(langOf("Makefile"), "plain");
  assert.equal(langOf(".gitignore"), "plain");
});
