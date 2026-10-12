import { test } from "node:test";
import assert from "node:assert/strict";
import { terminalView } from "../src/bundles/terminal_view/terminal_view.ts";

// A CANVAS TERMINAL is driven by the page alone: the exec agent it creates
// runs the child's own login shell (`shell: "login"`), found in the whole
// environment it inherits — the page names no program, the host writes nothing.

test("the exec agent a terminal spawns runs the child's own login shell", () => {
  assert.deepEqual(terminalView.backendFields, {
    cwd: ".",
    inherit_env: ["*"],
    shell: "login",
    ingress_rule: "allow_all_ingress",
  });
});

test("the view fronts exec_bridge and is granted exactly the verbs it calls", () => {
  assert.equal(terminalView.fronts, "exec_bridge.tools");
  assert.deepEqual(terminalView.grants("b1"), {
    b1: ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"],
  });
});
