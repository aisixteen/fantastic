import { test } from "node:test";
import assert from "node:assert/strict";
import { PreviewController, previews } from "../src/bundles/preview/preview_controller.ts";
import { TEXT_MAX, PAGE } from "../src/bundles/preview/preview_model.ts";
import { FakeBridge, settle } from "./fake_bridge.ts";
import type { Dict } from "./rig.ts";
import { project, rig } from "./rig.ts";

// THE PREVIEWER'S CONTROLLER: what it is bound to, the file it loads and holds
// live (and that a dispose ends every hold, even one still loading), the
// deleted-while-dirty and conflict states, the page of a large file, and the
// verbs an AI drives it with, refused in each way they can be.

/** A bridge that answers as the fake does, except where a test says otherwise. */
function overlay(b: FakeBridge, f: (p: Dict, next: () => Promise<Dict>) => Promise<Dict>): void {
  const was = b.call.bind(b);
  b.call = (p) => f(p, () => was(p));
}

test("deleted while dirty: the edits stay on screen, saving waits, and the file coming back is a conflict, never a blind write", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_dd", { pinned: "src/one.txt", follow: false });
  p.setEditing(true);
  p.edit("one, edited");
  r.bridge.remove("src/one.txt");
  await settle();
  assert.equal(p.gone, true);
  assert.equal(p.content.k, "text", "the edits are not replaced by the deleted card");
  assert.equal(p.editor.text(), "one, edited");
  assert.equal(p.dirty, true);
  assert.equal(p.whyNot(), "the file was deleted on disk");
  assert.equal(p.canEdit(), false);
  await p.save();
  assert.match(p.notice ?? "", /deleted on disk: saving waits/);
  assert.equal(r.bridge.files.has("src/one.txt"), false, "nothing was written");
  assert.equal((await r.call("pv_dd", { type: "save" }))["reason"], "file_deleted");

  r.bridge.put("src/one.txt", "elsewhere");
  await settle();
  assert.equal(p.gone, false);
  assert.equal(p.editor.state.k, "conflict", "the disk's text is a conflict to resolve, not a silent overwrite");
  assert.equal(r.bridge.files.get("src/one.txt"), "elsewhere", "the disk was not written");
  p.dispose();
});

test("deleted while not dirty shows the deleted card; the same path back shows its text, held all the while", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_del", { pinned: "src/one.txt", follow: false });
  r.bridge.remove("src/one.txt");
  await settle();
  assert.equal(p.content.k, "deleted");
  assert.equal(p.isLive(), true, "the hold is kept for when it comes back");
  r.bridge.put("src/one.txt", "back");
  await settle();
  assert.equal(p.content.k, "text");
  assert.equal(p.editor.text(), "back");
  p.dispose();
});

test("a dispose while the file is being looked up ends the hold it would take: nothing is left held on the bridge", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_gate", { pinned: "docs/a.md", follow: false });
  let release!: () => void;
  const gate = new Promise<void>((res) => (release = res));
  overlay(r.bridge, async (q, next) => {
    if (q["type"] === "list" && q["path"] === "src") await gate;
    return next();
  });
  const pending = p.pin("src/one.txt");
  p.dispose();
  release();
  await pending;
  await settle();
  assert.deepEqual([...r.bridge.watched].sort(), [], "the preview was disposed: its watch on the new file must not be left armed");
  assert.deepEqual(r.hub.held(), []);
});

test("a folder shows its entries, is live (a file made in it shows), and a refused listing is the error", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_dir", { pinned: "src", follow: false });
  assert.equal(p.mode, "dir");
  assert.equal(p.title(), "src · dir");
  assert.equal(p.canEdit(), false);
  assert.match(p.whyNot(), /folder/);
  if (p.content.k !== "dir") assert.fail("a folder shows its entries");
  assert.deepEqual(p.content.entries.map((e) => e.path).sort(), ["src/deep", "src/one.txt"]);
  r.bridge.put("src/new.txt", "n");
  await settle();
  assert.ok(p.content.k === "dir" && p.content.entries.some((e) => e.path === "src/new.txt"), "a change in the folder shows");
  overlay(r.bridge, async (q, next) => (q["type"] === "list" && q["path"] === "src" ? { error: "list: denied", reason: "denied" } : next()));
  await p.refresh();
  assert.equal(p.content.k, "error");
  assert.equal(p.mode, "error");
  p.dispose();
});

test("a large file pages: the page is clamped to the file, a binary page is the size card, a refused page is the error", async () => {
  const r = rig();
  project(r.bridge);
  r.bridge.put("big.txt", "ab".repeat(TEXT_MAX));
  const p = await r.mountPreview("pv_large", { pinned: "big.txt", follow: false });
  assert.equal(p.mode, "large");
  if (p.content.k !== "large") assert.fail("a large file is paged");
  assert.equal(p.content.page, 0);
  await p.page(999);
  if (p.content.k !== "large") assert.fail("still paged");
  const last = Math.ceil((2 * TEXT_MAX) / PAGE) - 1;
  assert.equal(p.content.page, last, "a page past the end is the last one");
  await p.page(-4);
  assert.ok(p.content.k === "large" && p.content.page === 0, "a page before the start is the first");

  r.bridge.put("bin.txt", "a\u0000" + "x".repeat(TEXT_MAX));
  const b = await r.mountPreview("pv_large_bin", { pinned: "bin.txt", follow: false });
  assert.equal(b.content.k, "binary", "a NUL in the page makes it the size card, not garbage");
  assert.equal(b.mode, "binary");

  overlay(r.bridge, async (q, next) => (q["type"] === "read_stream" && q["path"] === "big.txt" ? { error: "stream: io", reason: "io" } : next()));
  await p.page(1);
  assert.equal(p.content.k === "error" && p.content.error, "stream: io");
  p.dispose();
  b.dispose();
});

test("page with no file shown does nothing", async () => {
  const r = rig();
  const p = await r.mountPreview("pv_nopage", {});
  await p.page(3);
  assert.equal(p.content.k, "none");
  p.dispose();
});

test("a change to a followed text file that the bridge cannot look at any more keeps what is shown, and says why", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_look", { pinned: "src/one.txt", follow: false });
  let vanish = false;
  overlay(r.bridge, async (q, next) => (vanish && q["type"] === "list" && q["path"] === "src" ? { error: "list: gone", reason: "not_found" } : next()));
  vanish = true;
  r.bridge.put("src/one.txt", "changed while looked at");
  await settle();
  assert.equal(p.content.k, "text", "what is shown is not replaced by a look that failed");
  assert.equal(p.lastRefusal?.["reason"], "not_found");
  p.dispose();
});

test("a read that a newer read or another file overtakes is not applied to what is shown now", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_stale", { pinned: "src/one.txt", follow: false });
  let release!: () => void;
  const gate = new Promise<void>((res) => (release = res));
  overlay(r.bridge, async (q, next) => {
    if (q["type"] === "read" && q["path"] === "src/one.txt") await gate;
    return next();
  });
  const slow = p.exec([{ kind: "read" }]);
  await p.pin("docs/a.md");
  assert.equal(p.editor.text(), "# Hello\n");
  release();
  await slow;
  assert.equal(p.path, "docs/a.md");
  assert.equal(p.editor.text(), "# Hello\n", "the old file's read does not land on the new file");
  p.dispose();
});

test("a read that finds the file now binary shows the size card; a read refused for another reason is said, and the edits are kept", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_bin", { pinned: "src/one.txt", follow: false });
  r.bridge.binary.add("src/one.txt");
  await p.exec([{ kind: "read" }]);
  assert.equal(p.mode, "binary");
  assert.equal(p.content.k, "binary");
  r.bridge.binary.delete("src/one.txt");

  const q = await r.mountPreview("pv_io", { pinned: "top.txt", follow: false });
  overlay(r.bridge, async (x, next) => (x["type"] === "read" && x["path"] === "top.txt" ? { error: "read: io", reason: "io" } : next()));
  await q.exec([{ kind: "read" }]);
  assert.equal(q.notice, "read: io");
  assert.equal(q.lastRefusal?.["reason"], "io");
  p.dispose();
  q.dispose();
});

test("a deleted read while dirty keeps the edits and marks the file gone; saving waits for it", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_dgone", { pinned: "top.txt", follow: false });
  p.setEditing(true);
  p.edit("t, edited");
  r.bridge.remove("top.txt");
  await p.exec([{ kind: "read" }]);
  assert.equal(p.gone, true);
  assert.equal(p.content.k, "text");
  assert.equal(p.editor.text(), "t, edited");
  p.dispose();
});

test("setEditing: on only for an editable file; off only without unsaved edits; each refusal says why", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_edit", { pinned: "src/one.txt", follow: false });
  assert.equal(p.setEditing(true), null);
  assert.equal(p.editing, true);
  p.edit("x");
  assert.equal(p.setEditing(false), "unsaved edits: save or revert first");
  assert.equal(p.editing, true);
  await p.revert();
  assert.equal(p.setEditing(false), null);
  assert.equal(p.editing, false);

  r.bridge.binary.add("top.txt");
  const b = await r.mountPreview("pv_edit_bin", { pinned: "top.txt", follow: false });
  assert.equal(b.setEditing(true), "a binary file is not editable");
  assert.equal(b.editing, false);
  p.dispose();
  b.dispose();
});

test("save: a clean preview only redraws; dirty edits are written against the version read; a refused save is said on the preview", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_save", { pinned: "top.txt", follow: false });
  let drawn = 0;
  p.onChange(() => drawn++);
  await p.save();
  assert.equal(p.editor.state.k, "viewing");
  assert.ok(drawn > 0);
  p.setEditing(true);
  p.edit("t2");
  await p.save();
  assert.equal(r.bridge.files.get("top.txt"), "t2");
  assert.equal(p.editor.state.k, "viewing");
  p.setEditing(true);
  p.edit("t3");
  overlay(r.bridge, async (q, next) => (q["type"] === "write" ? { error: "write: read-only", reason: "readonly" } : next()));
  await p.save();
  assert.equal(p.editor.state.k, "dirty");
  assert.equal(p.editor.error, "write: read-only");
  assert.equal(p.lastRefusal?.["reason"], "readonly");
  p.dispose();
});

test("revert with nothing to revert changes nothing; with edits it drops them and says so", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_rev", { pinned: "top.txt", follow: false });
  await p.revert();
  assert.equal(p.editor.state.k, "viewing");
  p.setEditing(true);
  p.edit("gone");
  const g = p.editor.generation;
  await p.revert();
  assert.equal(p.editor.text(), "t");
  assert.ok(p.editor.generation > g, "the text on screen is drawn again");
  p.dispose();
});

test("the cell's own state: the title, the name, the source view, the pin, the mode of a folder", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_state", { pinned: "docs/a.md", follow: false });
  assert.equal(p.title(), "docs/a.md · markdown");
  assert.equal(p.name(), "a.md");
  assert.equal(p.isLive(), true);
  p.setSource(true);
  assert.equal(p.source, true);
  assert.equal(await p.setFileMode("agent"), null, "a file that is no agent: the mode is still set");
  assert.equal(p.fileMode, "agent");
  p.setEditing(true);
  p.edit("# Hi");
  assert.equal(await p.setFileMode("file"), "unsaved edits: save or revert first");
  assert.equal(p.fileMode, "agent", "refused: the mode did not move");
  p.dispose();
});

test("a binding that names nothing shows nothing: the title says so, and the name is empty", async () => {
  const r = rig();
  const p = await r.mountPreview("pv_none", {});
  assert.equal(p.title(), "nothing selected");
  assert.equal(p.name(), "");
  assert.equal(p.whyNot(), "nothing is shown");
  assert.equal(p.canEdit(), false);
  p.dispose();
});

test("the root of the project is named 'project' and the tree's selection of an agent with no folder shows the agent", async () => {
  const r = rig();
  project(r.bridge);
  const root = await r.mountPreview("pv_root", { pinned: "", follow: false });
  assert.equal(root.title(), "project · dir");
  root.dispose();
  const f = await r.mountPreview("pv_follow2", {});
  await r.select({ path: null, agent: "page_agent_1" });
  assert.equal(f.content.k, "agent");
  assert.equal(f.title(), "page_agent_1 · agent");
  assert.equal(f.isAgent, true);
  assert.equal(f.whyNot(), "agent mode shows the live agent; switch to file mode to edit its files");
  await r.select(null);
  assert.equal(f.content.k, "none");
  f.dispose();
});

test("a binding restored from the record: a pinned path is kept, a follow-false without a path is not, a path outside the project is not", async () => {
  const r = rig();
  const a = new PreviewController({ bridge: r.bridge, hub: r.hub, selfId: "pv_rs1", index: r.index, persist: () => {}, selection: () => null });
  a.restore({ pinned: "/etc/passwd" });
  assert.deepEqual(a.binding, { follow: true }, "an outside path is not restored");
  a.restore({ follow: false, path: "src/one.txt" });
  assert.deepEqual(a.binding, { pinned: "src/one.txt" });
  a.restore({ mode: "agent" });
  assert.equal(a.fileMode, "agent");
  const b = new PreviewController({ bridge: r.bridge, hub: r.hub, selfId: "pv_rs2", index: r.index, persist: () => {}, selection: () => null });
  b.restore({ follow: false });
  assert.deepEqual(b.binding, { follow: true }, "not pinned without a path");
});

test("after dispose a change on disk reaches nothing and the change listener is not called", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_off", { pinned: "top.txt", follow: false });
  let told = 0;
  p.onChange(() => told++);
  p.dispose();
  const before = told;
  r.bridge.put("top.txt", "after");
  await settle();
  assert.equal(told, before);
  assert.equal(p.editor.text(), "t", "nothing was loaded after dispose");
  assert.equal(r.bridge.watched.has("top.txt"), false);
});

test("the verbs: open, pin, unpin, mode, edit, save and revert each refuse what they must, by name", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_v", { pinned: "top.txt", follow: false });
  assert.equal((await r.call("pv_v", { type: "frobnicate" }))["reason"], "unhandled");
  assert.equal((await r.call("pv_v", { type: "" }))["reason"], "bad_arg", "a call with no verb is refused by the kernel");
  assert.equal((await r.call("pv_v", { type: "open", path: "/abs" }))["reason"], "outside_root");
  assert.equal((await r.call("pv_v", { type: "open" }))["reason"], "bad_arg");
  assert.equal((await r.call("pv_v", { type: "open", path: "nope.txt" }))["reason"], "not_found");
  assert.deepEqual(await r.call("pv_v", { type: "open", path: "top.txt" }), { opened: "top.txt", mode: "text" }, "open pins: back to the file");
  assert.equal((await r.call("pv_v", { type: "mode", mode: "agent" }))["reason"], "not_an_agent");
  assert.equal((await r.call("pv_v", { type: "mode", mode: "sideways" }))["reason"], "bad_arg");
  assert.equal((await r.call("pv_v", { type: "save" }))["reason"], "not_editing");
  assert.equal((await r.call("pv_v", { type: "revert" }))["reason"], "nothing_to_revert");
  assert.equal((await r.call("pv_v", { type: "edit", path: "/abs" }))["reason"], "outside_root");

  await r.call("pv_v", { type: "edit" });
  assert.equal(p.editing, true);
  assert.deepEqual(await r.call("pv_v", { type: "edit", path: "docs/a.md" }), { editing: "docs/a.md", version: (p.editor.version()) }, "a clean preview moves to the other file");
  await r.call("pv_v", { type: "open", path: "top.txt" });
  await r.call("pv_v", { type: "edit" });
  p.editor.edit("t!");
  assert.equal((await r.call("pv_v", { type: "open", path: "docs/a.md" }))["reason"], "unsaved_edits");
  assert.equal((await r.call("pv_v", { type: "edit", path: "docs/a.md" }))["reason"], "unsaved_edits");
  assert.equal((await r.call("pv_v", { type: "edit", path: "top.txt" }))["editing"], "top.txt", "the same file: no change of file");
  r.bridge.put("top.txt", "theirs");
  await settle();
  assert.equal((await r.call("pv_v", { type: "save" }))["reason"], "conflict");
  assert.equal((await r.call("pv_v", { type: "pin" }))["reason"], undefined, "pin with no path re-pins what is shown");
  assert.equal((await r.call("pv_v", { type: "pin", path: 9 }))["pinned"], "top.txt", "a path that is not a string is not a path");
  assert.equal((await r.call("pv_v", { type: "pin", path: "../x" }))["reason"], "outside_root");
  await p.revert();
  assert.deepEqual(await r.call("pv_v", { type: "unpin" }), { following: true });
  p.dispose();
  const nothing = await r.mountPreview("pv_v2", {});
  assert.equal((await r.call("pv_v2", { type: "pin" }))["reason"], "nothing_shown");
  assert.equal((await r.call("pv_v2", { type: "edit" }))["reason"], "nothing_shown");
  nothing.dispose();
});

test("the verbs: edit refuses what cannot be edited, saying why; save of a deleted file is refused by name", async () => {
  const r = rig();
  project(r.bridge);
  r.bridge.binary.add("top.txt");
  await r.mountPreview("pv_v3", { pinned: "top.txt", follow: false });
  const e = await r.call("pv_v3", { type: "edit" });
  assert.equal(e["reason"], "not_editable");
  assert.equal(e["why"], "a binary file is not editable");

  r.bridge.binary.delete("top.txt");
  const p = previews.get("pv_v3")!;
  await p.pin("src/one.txt");
  assert.equal((await r.call("pv_v3", { type: "edit" }))["editing"], "src/one.txt");
  p.editor.edit("one!");
  r.bridge.remove("src/one.txt");
  await settle();
  assert.equal((await r.call("pv_v3", { type: "save" }))["reason"], "file_deleted");
});

test("the verbs: a revert the bridge refuses is the bridge's refusal, said", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_undo", { pinned: "top.txt", follow: false });
  p.setEditing(true);
  p.edit("v2");
  await p.save();
  overlay(r.bridge, async (q, next) => (q["type"] === "undo" ? { error: "undo: nothing to undo", reason: "not_found" } : next()));
  const x = await r.call("pv_undo", { type: "revert" });
  assert.equal(x["reason"], "not_found");
  assert.equal(x["error"], "undo: nothing to undo");
  p.dispose();
});

test("the verbs: an unmounted cell is refused, and mode on a folder's agent switches it", async () => {
  const r = rig();
  project(r.bridge);
  assert.equal((await r.call("pv_ghost", { type: "open", path: "top.txt" }))["reason"], "unknown_agent", "a cell that is not registered is the kernel's refusal");
  const dir = await r.mountPreview("pv_agentdir", { pinned: ".fantastic/agents/user/agents/files", follow: false });
  r.index.set(".fantastic/agents/user/agents/files", { id: "files", handler: "file_bridge.tools", displayName: "", hasParent: false, hasMemory: false });
  await dir.refresh();
  assert.equal(dir.isAgent, true);
  assert.equal((await r.call("pv_agentdir", { type: "mode", mode: "agent" }))["mode"], "agent");
  assert.equal(dir.content.k, "agent");
  assert.equal(dir.path, ".fantastic/agents/user/agents/files");
  dir.dispose();
});

test("a restored preview that is disposed leaves nothing on the hub and writes nothing to the record", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_end", { pinned: "src/one.txt", follow: false });
  const meta: Dict[] = [];
  const again = new PreviewController({ bridge: r.bridge, hub: r.hub, selfId: "pv_end2", index: r.index, persist: (m) => meta.push({ ...m }), selection: () => null });
  again.restore({});
  p.dispose();
  assert.deepEqual(r.hub.held(), []);
  assert.equal(meta.length, 0);
  again.dispose();
});

test("the bridge that is missing a file's stat says the file is not in the project, and holds nothing for it", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_missing", { pinned: "missing/none.txt", follow: false });
  assert.equal(p.mode, "error");
  assert.equal(p.gone, true);
  assert.equal(p.content.k, "error");
  assert.match(p.content.k === "error" ? p.content.error : "", /does not exist/, "the bridge's own refusal is said");
  const later = await r.mountPreview("pv_later", { pinned: "src/later.txt", follow: false });
  assert.equal(later.content.k, "error", "a file not there yet: its folder is watched, the file shows when it appears");
  r.bridge.put("src/later.txt", "appeared");
  await settle();
  assert.equal(later.content.k, "text", "the held path shows when it appears");
  later.dispose();
  p.dispose();
});


// ─── the branches a race or an odd reply reaches ───────────────────────────

/** Holds the reply to the first call `match` picks until `release()`: the call itself is performed at once, as a real transport does. */
function gateReply(b: FakeBridge, match: (p: Dict) => boolean): { release: () => void } {
  let release!: () => void;
  const gate = new Promise<void>((res) => (release = res));
  let armed = true;
  overlay(b, async (q, next) => {
    const r = await next();
    if (armed && match(q)) {
      armed = false;
      await gate;
    }
    return r;
  });
  return { release };
}

test("a bind that a newer bind overtakes while it looks up the file shows nothing of its own: the newer one decides", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_race_stat", { pinned: "top.txt", follow: false });
  const g = gateReply(r.bridge, (q) => q["type"] === "list" && q["path"] === "docs");
  const slow = p.pin("docs/a.md");
  await p.pin("src/one.txt");
  g.release();
  await slow;
  await settle();
  assert.equal(p.path, "src/one.txt");
  assert.equal(p.editor.text(), "one", "the look-up that came back late changed nothing");
  p.dispose();
});

test("a bind overtaken while it takes its hold ends that hold: only the newer file is watched", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_race_hold", { pinned: "top.txt", follow: false });
  const g = gateReply(r.bridge, (q) => q["type"] === "watch_path" && q["path"] === "docs/a.md");
  const slow = p.pin("docs/a.md");
  await settle();
  await p.pin("src/one.txt");
  g.release();
  await slow;
  await settle();
  assert.equal(p.path, "src/one.txt");
  assert.deepEqual([...r.bridge.watched].sort(), ["src/one.txt"], "docs/a.md was armed by the late hold and then let go: nothing is left");
  p.dispose();
});

test("a folder listed late after the preview moved on shows nothing of the folder", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_race_dir", { pinned: "top.txt", follow: false });
  const g = gateReply(r.bridge, (q) => q["type"] === "list" && q["path"] === "src");
  const slow = p.pin("src");
  await settle();
  await p.pin("docs/a.md");
  g.release();
  await slow;
  assert.equal(p.path, "docs/a.md");
  assert.equal(p.editor.text(), "# Hello\n");
  p.dispose();
});

test("a text read that comes back after the preview moved on is not shown", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_race_read", { pinned: "top.txt", follow: false });
  const g = gateReply(r.bridge, (q) => q["type"] === "read" && q["path"] === "docs/a.md");
  const slow = p.pin("docs/a.md");
  await settle();
  await p.pin("src/one.txt");
  g.release();
  await slow;
  assert.equal(p.editor.text(), "one");
  p.dispose();
});

test("a page that comes back after a newer page is not shown: the newer page stands", async () => {
  const r = rig();
  project(r.bridge);
  r.bridge.put("big2.txt", "cd".repeat(TEXT_MAX));
  const p = await r.mountPreview("pv_race_page", { pinned: "big2.txt", follow: false });
  const g = gateReply(r.bridge, (q) => q["type"] === "read_stream" && q["offset"] === PAGE);
  const slow = p.page(1);
  await settle();
  await p.page(2);
  g.release();
  await slow;
  assert.ok(p.content.k === "large" && p.content.page === 2, "page 2 is what is shown");
  p.dispose();
});

test("a followed change whose look-up returns after the preview moved on applies nothing", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_race_follow", { pinned: "src/one.txt", follow: false });
  const g = gateReply(r.bridge, (q) => q["type"] === "list" && q["path"] === "src");
  r.bridge.put("src/one.txt", "changed");
  await settle();
  await p.pin("docs/a.md");
  g.release();
  await settle();
  assert.equal(p.path, "docs/a.md");
  assert.equal(p.editor.text(), "# Hello\n");
  p.dispose();
});

test("a followed change whose look-up returns after dispose applies nothing, and leaves no hold", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_race_follow_d", { pinned: "src/one.txt", follow: false });
  const g = gateReply(r.bridge, (q) => q["type"] === "list" && q["path"] === "src");
  r.bridge.put("src/one.txt", "changed");
  await settle();
  p.dispose();
  g.release();
  await settle();
  assert.equal(p.editor.text(), "one");
  assert.deepEqual([...r.bridge.watched], []);
});

test("a folder's listing that carries no files is an empty folder; a stat that finds no files is not in the project", async () => {
  const r = rig();
  project(r.bridge);
  overlay(r.bridge, async (q, next) => (q["type"] === "list" && (q["path"] === "src" || q["path"] === "docs") ? { path: q["path"] } : next()));
  const d = await r.mountPreview("pv_nofiles_dir", { pinned: "src", follow: false });
  assert.ok(d.content.k === "dir" && d.content.entries.length === 0);
  const f = await r.mountPreview("pv_nofiles_file", { pinned: "docs/a.md", follow: false });
  assert.match(f.content.k === "error" ? f.content.error : "", /not in the project/);
  d.dispose();
  f.dispose();
});

test("a text file the bridge refuses for another reason is the error card, with the reason said", async () => {
  const r = rig();
  project(r.bridge);
  overlay(r.bridge, async (q, next) => (q["type"] === "read" && q["path"] === "top.txt" ? { error: "read: io failure", reason: "io" } : next()));
  const p = await r.mountPreview("pv_io_load", { pinned: "top.txt", follow: false });
  assert.equal(p.mode, "error");
  assert.deepEqual(p.content, { k: "error", error: "read: io failure" });
  p.dispose();
});

test("a zip is a binary file by its extension: the size card, not read", async () => {
  const r = rig();
  project(r.bridge);
  r.bridge.put("pack.zip", "PK\u0003\u0004");
  const p = await r.mountPreview("pv_zip", { pinned: "pack.zip", follow: false });
  assert.equal(p.mode, "binary");
  assert.deepEqual(p.content, { k: "binary", size: 4 });
  assert.equal(p.title(), "pack.zip · binary");
  p.dispose();
});

test("a page whose bytes are not bytes is the size card", async () => {
  const r = rig();
  project(r.bridge);
  r.bridge.put("big3.txt", "ef".repeat(TEXT_MAX));
  const p = await r.mountPreview("pv_nonbytes", { pinned: "big3.txt", follow: false });
  overlay(r.bridge, async (q, next) => (q["type"] === "read_stream" && q["offset"] === PAGE ? { size: 2 * TEXT_MAX, bytes: 42, eof: false } : next()));
  await p.page(1);
  assert.deepEqual(p.content, { k: "binary", size: 2 * TEXT_MAX });
  p.dispose();
});

test("a sibling in the folder that is held changes nothing of the file shown", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_sibling", { pinned: "src/one.txt", follow: false });
  const before = r.bridge.calls.length;
  r.bridge.put("src/other.txt", "x");
  await settle();
  assert.equal(p.editor.text(), "one");
  assert.equal(p.gone, false);
  assert.equal(r.bridge.calls.slice(before).filter((c) => c["type"] === "read").length, 0, "the file shown was not read again");
  p.dispose();
});

test("a followed file whose listing carries no size is looked at as the text it is", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_nosize", { pinned: "src/one.txt", follow: false });
  overlay(r.bridge, async (q, next) => {
    if (q["type"] === "list" && q["path"] === "src") {
      const x = (await next()) as { files: Array<Record<string, unknown>> };
      return { ...x, files: x.files.map((f) => (f["path"] === "src/one.txt" ? { ...f, size: undefined } : f)) };
    }
    return next();
  });
  r.bridge.put("src/one.txt", "still text");
  await settle();
  assert.equal(p.editor.text(), "still text");
  assert.equal(p.mode, "text");
  p.dispose();
});

test("a preview with nothing shown is not written into or saved, and an exec of nothing changes nothing", async () => {
  const r = rig();
  const q = await r.mountPreview("pv_nopath", {});
  await q.exec([{ kind: "read" }]);
  assert.equal(q.content.k, "none");
  await q.exec([{ kind: "write", bytes: "x", ifMatch: "v" }]);
  assert.equal(r.bridge.calls.filter((c) => c["type"] === "write").length, 0);
  q.dispose();
});

test("a read that finds the file gone while it is not dirty shows the deleted card", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_silent_gone", { pinned: "top.txt", follow: false });
  r.bridge.files.delete("top.txt");
  await p.exec([{ kind: "read" }]);
  assert.equal(p.gone, true);
  assert.equal(p.content.k, "deleted");
  p.dispose();
});

test("the mode verb is refused with unsaved edits, and the mode stays", async () => {
  const r = rig();
  project(r.bridge);
  r.index.set(".fantastic/agents/user/agents/files", { id: "files", handler: "file_bridge.tools", displayName: "", hasParent: false, hasMemory: false });
  const p = await r.mountPreview("pv_mode_dirty", { pinned: ".fantastic/agents/user/agents/files", follow: false });
  assert.equal(p.isAgent, true);
  p.setEditing(true);
  p.edit("name: changed\n");
  assert.equal((await r.call("pv_mode_dirty", { type: "mode", mode: "agent" }))["reason"], "unsaved_edits");
  assert.equal(p.fileMode, "file");
  p.dispose();
});

test("save: a disk change that lands as the write goes out is a conflict in the reply, with both versions", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_silent_conflict", { pinned: "top.txt", follow: false });
  await r.call("pv_silent_conflict", { type: "edit" });
  p.edit("mine");
  let once = true;
  overlay(r.bridge, async (q, next) => {
    if (q["type"] === "write" && once) {
      once = false;
      r.bridge.files.set("top.txt", "silent"); // changed on disk without a word to the watch
    }
    return next();
  });
  const x = await r.call("pv_silent_conflict", { type: "save" });
  assert.equal(x["reason"], "conflict");
  assert.equal(typeof x["expected"], "string");
  assert.equal(typeof x["actual"], "string");
  assert.equal(p.editor.state.k, "conflict");
  p.dispose();
});

test("save: a write the bridge refuses (not a mismatch) is the bridge's refusal in the reply", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_refused_save", { pinned: "top.txt", follow: false });
  await r.call("pv_refused_save", { type: "edit" });
  p.edit("mine");
  overlay(r.bridge, async (q, next) => (q["type"] === "write" ? { error: "write: read-only", reason: "readonly" } : next()));
  const x = await r.call("pv_refused_save", { type: "save" });
  assert.equal(x["reason"], "readonly");
  assert.equal(x["error"], "write: read-only");
  p.dispose();
});

test("save: with an error the editor holds and no refusal of the bridge, the error is the reply", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv_bare_error", { pinned: "top.txt", follow: false });
  await r.call("pv_bare_error", { type: "edit" });
  p.editor.error = "the editor says no";
  p.lastRefusal = null;
  const x = await r.call("pv_bare_error", { type: "save" });
  assert.equal(x["error"], "the editor says no");
  assert.equal(x["reason"], "refused");
  p.dispose();
});

test("a follow-bound preview shows whatever the tree selects, and nothing when the selection goes", async () => {
  const r = rig();
  project(r.bridge);
  const sel: Array<{ path: string | null; agent: string | null } | null> = [null];
  const p = new PreviewController({ bridge: r.bridge, hub: r.hub, selfId: "pv_sel_fn", index: r.index, persist: () => {}, selection: () => sel[0] });
  await p.refresh();
  assert.equal(p.path, null);
  sel[0] = { path: "top.txt", agent: null };
  await p.refresh();
  assert.equal(p.path, "top.txt");
  p.dispose();
});
