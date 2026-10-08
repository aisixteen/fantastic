import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { SELECTOR, SELECTOR_TITLE, registerSelector, setSelectorHost } from "../src/bundles/canvas/selector_agent.ts";
import { tiles } from "../src/bundles/canvas/action_popup.ts";

test("a selector is added at the spot, opens on boot with its title, says what it offers, and deletes itself when done", async () => {
  const k = new Kernel();
  registerSelector(k);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "view.ts" }));
  let close: (() => void) | null = null;
  const opened: Array<[string, number | null, number | null, string]> = [];
  setSelectorHost({
    open: async (id, sx, sy, t, done) => {
      opened.push([id, sx, sy, t]);
      close = done;
    },
    tiles: async (typed) => tiles(typed, null).map((t) => ({ label: t.label, kind: t.pick.kind, section: t.section, hint: t.hint })),
  });
  k.register(new Agent({ id: "selector_1", parentId: "view_root", handlerModule: SELECTOR, meta: { x: 5, y: 6, sx: 10, sy: 20 } }));
  const r = (await k.send([{ target: "selector_1", payload: { type: "boot" } as unknown as Payload }]))[0]!.result as Payload;
  assert.deepEqual(r, { opened: true, title: SELECTOR_TITLE });
  assert.deepEqual(opened, [["selector_1", 10, 20, "Fantastic Canvas"]], "opened once, at its screen spot");
  const t = ((await k.send([{ target: "selector_1", payload: { type: "tiles" } as unknown as Payload }]))[0]!.result as Payload)["tiles"] as Array<{ label: string; kind: string }>;
  assert.ok(t.some((x) => x.kind === "terminal"));
  assert.ok(!t.some((x) => x.kind === "selector"), "it never offers itself");
  assert.ok(k.get("selector_1") !== undefined);
  close!();
  assert.equal(k.get("selector_1"), undefined, "done → it deleted itself");
});
