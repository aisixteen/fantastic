import { test } from "node:test";
import assert from "node:assert/strict";
import { linkKind, parseInline, parseMarkdown, resolveRel } from "../src/bundles/preview/markdown.ts";

test("blocks: heading, paragraph, rule, quote, fenced code (unterminated runs to the end)", () => {
  const ast = parseMarkdown("# Title\n\nsome *text*\nover two lines\n\n---\n\n> quoted\n\n```ts\nlet a = 1;\n```\n\n~~~\nopen fence");
  assert.deepEqual(ast.map((b) => b.t), ["heading", "para", "hr", "quote", "code", "code"]);
  assert.deepEqual(ast[0], { t: "heading", level: 1, c: [{ t: "text", v: "Title" }] });
  assert.deepEqual(ast[4], { t: "code", lang: "ts", v: "let a = 1;" });
  assert.deepEqual(ast[5], { t: "code", lang: "", v: "open fence" });
  const para = ast[1] as { c: unknown[] };
  assert.deepEqual(para.c[0], { t: "text", v: "some " });
});

test("lists: bullets, ordered, nested by indent", () => {
  const ast = parseMarkdown("- a\n- b\n  - b1\n  - b2\n- c\n\n1. one\n2. two\n");
  assert.equal(ast.length, 2);
  const ul = ast[0] as { t: "list"; ordered: boolean; items: Array<Array<{ t: string }>> };
  assert.equal(ul.ordered, false);
  assert.equal(ul.items.length, 3);
  assert.deepEqual(ul.items[1]!.map((b) => b.t), ["para", "list"]);
  const nested = ul.items[1]![1] as unknown as { items: unknown[] };
  assert.equal(nested.items.length, 2);
  assert.equal((ast[1] as { ordered: boolean }).ordered, true);
});

test("a simple table", () => {
  const ast = parseMarkdown("| a | b |\n|---|:-:|\n| 1 | **2** |\n| 3 | 4 |\n");
  assert.equal(ast.length, 1);
  const t = ast[0] as { t: "table"; head: unknown[][]; rows: unknown[][][] };
  assert.equal(t.t, "table");
  assert.equal(t.head.length, 2);
  assert.equal(t.rows.length, 2);
  assert.deepEqual(t.rows[0]![1], [{ t: "strong", c: [{ t: "text", v: "2" }] }]);
});

test("inline: em, strong, code, link, image, escapes; snake_case is not emphasis", () => {
  assert.deepEqual(parseInline("a *b* **c** `d` [e](x/y.md) ![f](i.png) \\*g"), [
    { t: "text", v: "a " },
    { t: "em", c: [{ t: "text", v: "b" }] },
    { t: "text", v: " " },
    { t: "strong", c: [{ t: "text", v: "c" }] },
    { t: "text", v: " " },
    { t: "code", v: "d" },
    { t: "text", v: " " },
    { t: "link", href: "x/y.md", c: [{ t: "text", v: "e" }] },
    { t: "text", v: " " },
    { t: "image", src: "i.png", alt: "f" },
    { t: "text", v: " *g" },
  ]);
  assert.deepEqual(parseInline("snake_case_name"), [{ t: "text", v: "snake_case_name" }]);
});

test("link schemes: only http(s), mailto and relative are followed; javascript: and friends are dropped", () => {
  assert.equal(linkKind("https://a.b/c"), "web");
  assert.equal(linkKind("http://a.b"), "web");
  assert.equal(linkKind("mailto:a@b.c"), "mail");
  assert.equal(linkKind("docs/a.md"), "file");
  assert.equal(linkKind("../a.md"), "file");
  assert.equal(linkKind("#frag"), "anchor");
  for (const bad of ["javascript:alert(1)", "JavaScript:alert(1)", "data:text/html,x", "file:///etc/passwd", "//evil.example/x", "/etc/passwd", "vbscript:x", ""]) {
    assert.equal(linkKind(bad), null, bad);
  }
});

test("resolveRel: from the file's folder, never out of the project", () => {
  assert.equal(resolveRel("docs", "a.md"), "docs/a.md");
  assert.equal(resolveRel("docs/x", "../b.md#h"), "docs/b.md");
  assert.equal(resolveRel("", "./a.md"), "a.md");
  assert.equal(resolveRel("docs", "../../etc/passwd"), null);
  assert.equal(resolveRel("", "../x"), null);
});
