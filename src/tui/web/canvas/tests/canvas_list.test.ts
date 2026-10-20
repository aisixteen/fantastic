import { test } from "node:test";
import assert from "node:assert/strict";
import { canvasesOf } from "../src/canvas_list.ts";

const node = (id: string, parent: string, extra: Record<string, unknown> = {}) => ({ id, parent_id: parent, handler_module: "canvas.tools", ...extra });
const loader = (canvas: string, root: string) => ({ id: `${canvas}_loader`, parent_id: canvas, handler_module: "kernel_state.tools", root });

test("canvasesOf lists peer canvases and canvases nested under a canvas (#132), main first", () => {
  const all = [
    { id: "canvas_page", parent_id: "web", handler_module: "canvas_page.tools" },
    node("canvas_root", "canvas_page", { main: true }),
    { id: "web_loader", parent_id: "canvas_root", handler_module: "kernel_state.tools", root: ".fantastic/web" },
    node("canvas_research", "canvas_page"),
    loader("canvas_research", ".fantastic/web/canvas_research"),
    node("canvas_cabinet_a", "canvas_root", { display_name: "alpha" }),
    loader("canvas_cabinet_a", ".fantastic/web/canvas_cabinet_a"),
    node("canvas_deep", "canvas_cabinet_a"),
    loader("canvas_deep", ".fantastic/web/canvas_cabinet_a/canvas_deep"),
  ];
  assert.deepEqual(
    canvasesOf(all).map((c) => [c.id, c.loader, c.main, c.name]),
    [
      ["canvas_root", "web_loader", true, "root"],
      ["canvas_cabinet_a", "canvas_cabinet_a_loader", false, "alpha"],
      ["canvas_deep", "canvas_deep_loader", false, "deep"],
      ["canvas_research", "canvas_research_loader", false, "research"],
    ],
  );
});

test("canvasesOf skips a canvas node outside canvas_page, one without a loader, and survives a parent cycle", () => {
  const all = [
    { id: "canvas_page", parent_id: "web", handler_module: "canvas_page.tools" },
    node("canvas_stray", "elsewhere"),
    loader("canvas_stray", "x"),
    node("canvas_bare", "canvas_page"),
    node("canvas_a", "canvas_b"),
    node("canvas_b", "canvas_a"),
    loader("canvas_a", "a"),
  ];
  assert.deepEqual(canvasesOf(all), []);
});

test("canvasesOf is loud about a loader that names no root", () => {
  const all = [{ id: "canvas_page", parent_id: "web" }, node("canvas_x", "canvas_page"), loader("canvas_x", "")];
  assert.throws(() => canvasesOf(all), /canvas_x: its page loader canvas_x_loader names no root/);
});
