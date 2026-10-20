import { test } from "node:test";
import assert from "node:assert/strict";
import { CONNECTOR_SRC } from "../src/bundles/html_agent/connector.ts";
import { canvasUrl } from "../src/bundles/html_agent/html_view.ts";

test("a panel opens a canvas by id: the page URL with ?canvas=<id>, the rest of the page kept", () => {
  const page = "http://127.0.0.1:8123/canvas_page/file/index.html?canvas=canvas_root#frag";
  assert.equal(canvasUrl(page, "canvas_cabinet_a"), "http://127.0.0.1:8123/canvas_page/file/index.html?canvas=canvas_cabinet_a");
  // the main page, with no canvas named yet
  assert.equal(canvasUrl("http://h/canvas_page/file/index.html", "canvas_x"), "http://h/canvas_page/file/index.html?canvas=canvas_x");
  // an id with characters a URL must escape stays one parameter
  assert.equal(new URL(canvasUrl("http://h/p", "a b&c")).searchParams.get("canvas"), "a b&c");
});

test("the connector exposes openCanvas: it posts the canvas id to the page, which opens it", () => {
  assert.match(CONNECTOR_SRC, /openCanvas\(id\) \{ post\(\{ op: "open_canvas", target: String\(id\) \}\); \}/);
});
