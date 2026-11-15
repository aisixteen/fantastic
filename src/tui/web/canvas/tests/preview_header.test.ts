import { test } from "node:test";
import assert from "node:assert/strict";
import { crumbs, kindOf, lineCount, metaOf, sizeText } from "../src/bundles/preview/header_model.ts";

test("crumbs: the path's segments, the root named project, nothing for no path", () => {
  assert.deepEqual(crumbs("deploy/api/api.yaml"), ["deploy", "api", "api.yaml"]);
  assert.deepEqual(crumbs(""), ["project"]);
  assert.deepEqual(crumbs(null), []);
});

test("kindOf: folder, file, agent", () => {
  assert.equal(kindOf("dir", false), "folder");
  assert.equal(kindOf("text", false), "file");
  assert.equal(kindOf("text", true), "agent");
});

test("sizeText and lineCount", () => {
  assert.equal(sizeText(1), "1 byte");
  assert.equal(sizeText(612), "612 bytes");
  assert.equal(sizeText(2048), "2.0 KiB");
  assert.equal(sizeText(-1), "unknown size");
  assert.equal(lineCount(""), 0);
  assert.equal(lineCount("a"), 1);
  assert.equal(lineCount("a\nb\n"), 2);
  assert.equal(lineCount("a\n\nb"), 3);
});

test("metaOf: what is known, in order", () => {
  assert.equal(metaOf({ name: "api.yaml", size: 612, text: "a\nb\n" }), "2 lines · 612 bytes · yaml");
  assert.equal(metaOf({ name: "Makefile", size: 10 }), "10 bytes");
  assert.equal(metaOf({ name: "x.png", size: -1 }), "png");
});
