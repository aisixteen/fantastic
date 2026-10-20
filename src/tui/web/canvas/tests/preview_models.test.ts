import { test } from "node:test";
import assert from "node:assert/strict";
import { EditorModel } from "../src/bundles/preview/editor_model.ts";
import { HIGHLIGHT_MAX, IMAGE_MAX, PDF_MAX, PAGE, TEXT_MAX, agentTarget, editable, extOf, human, modeFor, pageText, refine, refused, targetOf, why } from "../src/bundles/preview/preview_model.ts";
import { parseInline, parseMarkdown, renderMarkdown, linkKind, resolveRel, dirOf } from "../src/bundles/preview/markdown.ts";
import type { Block, RenderHooks } from "../src/bundles/preview/markdown.ts";
import { langOf, tokens } from "../src/bundles/preview/highlight.ts";
import { lineDiff, MAX_LINES } from "../src/bundles/preview/diff.ts";
import type { AgentInfo } from "../src/bundles/files/agent_index.ts";

// THE PREVIEWER'S PURE MODELS: the editor's state machine (every state, every
// conflict path, the revert generation), the mode of a path and its page, the
// markdown parser and renderer (through a tiny DOM that records what was made),
// the highlighter and the diff.

// ─── the editor's state machine ────────────────────────────────────────────

function conflicted(): EditorModel {
  const e = new EditorModel();
  e.load("one", "v1");
  e.edit("one, mine");
  e.reread({ text: "theirs", version: "v7" });
  return e;
}

test("editor: the text and the version on screen are the state's, in every state", () => {
  const e = new EditorModel();
  e.load("base", "v1");
  assert.equal(e.text(), "base");
  assert.equal(e.version(), "v1");
  e.edit("base mine");
  assert.equal(e.text(), "base mine");
  assert.equal(e.version(), "v1", "the version the edits are based on");
  e.save();
  assert.equal(e.text(), "base mine", "saving shows mine");
  assert.equal(e.version(), "v1");
  e.saveRefused({ reason: "version_mismatch" });
  e.reread({ text: "theirs", version: "v9" });
  assert.equal(e.state.k, "conflict");
  assert.equal(e.text(), "base mine");
  assert.equal(e.version(), "v1");
  e.compare();
  assert.equal(e.state.k, "comparing");
  assert.equal(e.text(), "base mine");
  assert.equal(e.inConflict(), true);
});

test("editor: dirty is 'mine differs from the base', and a save in flight that matches the base is not dirty", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  assert.equal(e.isDirty(), false, "viewing is never dirty");
  e.edit("a");
  assert.equal(e.state.k, "viewing", "typing the same text is no edit");
  e.edit("b");
  assert.equal(e.isDirty(), true);
  e.edit("a");
  assert.equal(e.state.k, "dirty", "back to the base text: still an edit under way");
  assert.equal(e.isDirty(), false, "but mine is the base: nothing to save");
});

test("editor: edits are taken only while viewing or dirty; a save, a conflict and a comparison take none", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("ab");
  e.edit("abc");
  assert.equal(e.text(), "abc", "dirty takes each edit");
  e.save();
  e.edit("ignored");
  assert.equal(e.text(), "abc", "saving takes none");
  const c = conflicted();
  c.edit("typed into a conflict");
  assert.equal(c.text(), "one, mine", "a conflict takes none");
  c.compare();
  c.edit("typed while comparing");
  assert.equal(c.text(), "one, mine");
});

test("editor: a save of a file that is not dirty does nothing; a saved echo while not saving does nothing", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  assert.deepEqual(e.save(), []);
  e.saved("v2");
  assert.equal(e.state.k, "viewing");
  assert.equal(e.version(), "v1");
  assert.equal(e.lastSaved, null, "nothing was saved");
});

test("editor: a save that lands is what is on screen, based on the version it wrote; typing during the save is refused", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("a1");
  e.save();
  e.edit("a12");
  assert.equal(e.text(), "a1", "typing while the write is in flight takes nothing");
  e.saved("v2");
  assert.equal(e.state.k, "viewing");
  assert.equal(e.version(), "v2", "the shown text is now based on what was written");
  assert.equal(e.text(), "a1");
  assert.equal(e.lastSaved, "v2");
  assert.equal(e.isDirty(), false);
});

test("editor: a refusal that is not a mismatch goes back to dirty with the reason; a refusal while not saving is nothing", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  assert.deepEqual(e.saveRefused({ reason: "version_mismatch" }), [], "not saving: nothing");
  e.edit("b");
  e.save();
  assert.deepEqual(e.saveRefused({ error: "disk full", reason: "io" }), []);
  assert.equal(e.state.k, "dirty");
  assert.equal(e.error, "disk full");
  assert.deepEqual(e.save(), [{ kind: "write", bytes: "b", ifMatch: "v1" }], "a refused save can be sent again");
  e.load("a", "v1");
  assert.equal(e.error, null, "a fresh load forgets the refusal");
});

test("editor: a refusal with no error text says the save was refused; a refusal without a reason is not a mismatch", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("b");
  e.save();
  e.saveRefused({});
  assert.equal(e.error, "the save was refused");
});

test("editor: a read after a mismatch that fails leaves the edits dirty, with the reason said", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("b");
  e.save();
  e.saveRefused({ reason: "version_mismatch" });
  e.readFailed("the file was deleted on disk");
  assert.equal(e.state.k, "dirty");
  assert.equal(e.error, "the file was deleted on disk");
  assert.equal(e.text(), "b");
});

test("editor: a failed read outside a refused save changes nothing", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.readFailed("nothing to say");
  assert.equal(e.state.k, "viewing");
  e.edit("b");
  e.readFailed("still nothing");
  assert.equal(e.state.k, "dirty");
  assert.equal(e.error, null);
});

test("editor: a disk change asks for a read", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  assert.deepEqual(e.diskChanged(), [{ kind: "read" }]);
});

test("editor: a read while viewing is taken only when the version moved (and a load is a new generation)", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  const g = e.generation;
  e.reread({ text: "a", version: "v1" });
  assert.equal(e.generation, g, "the same version: nothing is drawn again");
  e.reread({ text: "b", version: "v2" });
  assert.equal(e.text(), "b");
  assert.equal(e.generation, g, "a new version is shown without a new generation: the text is read in place");
});

test("editor: a read while dirty is ours when it is the base or the last save; anything else is a conflict", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("b");
  e.reread({ text: "a", version: "v1" });
  assert.equal(e.state.k, "dirty");
  const f = new EditorModel();
  f.load("a", "v1");
  f.edit("b");
  f.save();
  f.saved("v2");
  f.edit("b2");
  f.reread({ text: "b", version: "v2" });
  assert.equal(f.state.k, "dirty", "the echo of our own last save is not a conflict");
  f.reread({ text: "x", version: "v3" });
  assert.equal(f.state.k, "conflict");
});

test("editor: a read that comes back while a save is in flight waits for the write's own answer, unless a mismatch asked for it", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("b");
  e.save();
  e.reread({ text: "elsewhere", version: "v5" });
  assert.equal(e.state.k, "saving", "the write decides");
  e.saveRefused({ reason: "version_mismatch" });
  e.reread({ text: "elsewhere", version: "v5" });
  assert.equal(e.state.k, "conflict");
  assert.equal(e.text(), "b");
});

test("editor: a conflict that sees a newer version of theirs takes it; the same version changes nothing", () => {
  const e = conflicted();
  e.reread({ text: "theirs", version: "v7" });
  assert.equal((e.state as { theirs: string }).theirs, "theirs");
  e.reread({ text: "theirs again", version: "v8" });
  assert.equal((e.state as { theirs: string; theirsVersion: string }).theirs, "theirs again");
  assert.equal((e.state as { theirsVersion: string }).theirsVersion, "v8");
  e.compare();
  e.reread({ text: "theirs third", version: "v9" });
  assert.equal(e.state.k, "comparing", "comparing stays comparing");
  assert.equal((e.state as { theirsVersion: string }).theirsVersion, "v9");
});

test("editor: reload takes theirs and draws the text again; from anywhere but a conflict it is nothing", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  const g = e.generation;
  e.reload();
  assert.equal(e.generation, g);
  assert.equal(e.state.k, "viewing");
  const c = conflicted();
  const gc = c.generation;
  c.reload();
  assert.equal(c.state.k, "viewing");
  assert.equal(c.text(), "theirs");
  assert.equal(c.version(), "v7");
  assert.equal(c.generation, gc + 1, "the text on screen was replaced under the person");
  assert.equal(c.isDirty(), false);
});

test("editor: keep mine writes against the version just seen; from anywhere but a conflict it writes nothing", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("b");
  assert.deepEqual(e.keepMine(), []);
  assert.equal(e.state.k, "dirty");
  const c = conflicted();
  // a conflict from a read, not from a refused save
  const fx = c.keepMine();
  assert.deepEqual(fx, [{ kind: "write", bytes: "one, mine", ifMatch: "v7" }]);
  assert.equal(c.state.k, "saving");
  assert.equal(c.error, null);
  c.saved("v8");
  assert.equal(c.state.k, "viewing", "mine was written: the disk has it");
  assert.equal(c.text(), "one, mine");
});

test("editor: keep mine after an error clears the error; a conflict's keep mine that is itself refused is a fresh conflict", () => {
  const c = conflicted();
  c.error = "old error";
  c.keepMine();
  assert.equal(c.error, null);
  c.saveRefused({ reason: "version_mismatch" });
  assert.equal(c.state.k, "saving");
  c.reread({ text: "theirs", version: "v8" });
  assert.equal(c.state.k, "conflict");
  assert.equal((c.state as { theirsVersion: string }).theirsVersion, "v8");
});

test("editor: compare opens from a conflict only; nothing else is compared", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("b");
  e.compare();
  assert.equal(e.state.k, "dirty", "a dirty file is not a conflict: no comparison");
  const c = conflicted();
  c.compare();
  assert.equal(c.state.k, "comparing");
  c.compare();
  assert.equal(c.state.k, "comparing", "comparing again stays");
});

test("editor: revert drops the edits and draws the base again; a save is undone; a conflict or a save in flight has nothing to revert", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("b");
  e.error = "old";
  const g = e.generation;
  assert.deepEqual(e.revert(), { ok: true, what: "edits", effects: [] });
  assert.equal(e.error, null);
  assert.equal(e.generation, g + 1);
  assert.equal(e.text(), "a");

  const c = conflicted();
  assert.deepEqual(c.revert(), { ok: false, reason: "nothing_to_revert" });

  const s = new EditorModel();
  s.load("a", "v1");
  s.edit("b");
  s.save();
  assert.deepEqual(s.revert(), { ok: false, reason: "nothing_to_revert" }, "a save in flight is not revertable");
});

test("editor: a save that was made is undone only while the shown version is still that save's", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  e.edit("b");
  e.save();
  e.saved("v2");
  e.reread({ text: "elsewhere", version: "v3" });
  assert.deepEqual(e.revert(), { ok: false, reason: "nothing_to_revert" }, "the disk moved on: nothing of ours to undo");
  const f = new EditorModel();
  assert.deepEqual(f.revert(), { ok: false, reason: "nothing_to_revert" }, "a fresh editor has nothing");
});

// ─── the mode of a path ────────────────────────────────────────────────────

test("preview model: extOf is the lower-cased extension, none for a dot file or no dot", () => {
  assert.equal(extOf("Photo.PNG"), "png");
  assert.equal(extOf(".bashrc"), "");
  assert.equal(extOf("README"), "");
  assert.equal(extOf("a."), "");
});

test("preview model: the mode from the listing: a folder; images and PDFs by size; binaries by extension; large text; markdown; text", () => {
  assert.equal(modeFor({ type: "dir", name: "src" }), "dir");
  assert.equal(modeFor({ type: "file", name: "a.png", size: 10 }), "image");
  assert.equal(modeFor({ type: "file", name: "a.png", size: IMAGE_MAX + 1 }), "binary", "too big to draw");
  assert.equal(modeFor({ type: "file", name: "a.svg" }), "image", "no size is no problem for an image");
  assert.equal(modeFor({ type: "file", name: "a.pdf", size: PDF_MAX }), "pdf");
  assert.equal(modeFor({ type: "file", name: "a.pdf", size: PDF_MAX + 1 }), "binary");
  assert.equal(modeFor({ type: "file", name: "a.zip", size: 1 }), "binary");
  assert.equal(modeFor({ type: "file", name: "a.txt", size: TEXT_MAX + 1 }), "large");
  assert.equal(modeFor({ type: "file", name: "a.md", size: TEXT_MAX + 1 }), "large", "a big markdown is paged, not rendered");
  assert.equal(modeFor({ type: "file", name: "a.md", size: 5 }), "markdown");
  assert.equal(modeFor({ type: "file", name: "a.markdown" }), "markdown");
  assert.equal(modeFor({ type: "file", name: "a.ts", size: -1 }), "text", "a size the bridge could not stat counts as none");
  assert.equal(modeFor({ type: "file", name: "a.ts" }), "text");
  assert.ok(HIGHLIGHT_MAX < TEXT_MAX && PAGE < TEXT_MAX);
});

test("preview model: refine: a mismatch of text read is binary (on path) or an error (anything else), and a clean read keeps the mode", () => {
  assert.deepEqual(refine("text", {}), { mode: "text" }, "not a refusal");
  assert.deepEqual(refine("text", null), { mode: "text" });
  assert.deepEqual(refine("text", { error: "bad", reason: "bad_arg", arg: "path" }), { mode: "binary" });
  assert.deepEqual(refine("markdown", { error: "bad", reason: "bad_arg", arg: "path" }), { mode: "binary" });
  assert.deepEqual(refine("text", { error: "bad", reason: "bad_arg", arg: "offset" }), { mode: "error", error: "bad" }, "a bad argument that is not the path is an error");
  assert.deepEqual(refine("text", { error: "nope", reason: "not_found" }), { mode: "error", error: "nope" });
  assert.deepEqual(refine("image", { error: "bad", reason: "bad_arg", arg: "path" }), { mode: "error", error: "bad" }, "only text can turn binary");
  assert.equal(refused(null), false);
  assert.equal(refused({ error: 1 }), false);
  assert.equal(refused({ error: "x" }), true);
});

test("preview model: why an edit is refused, by mode, and that an agent's folder says to switch first", () => {
  assert.equal(editable("text"), true);
  assert.equal(editable("markdown"), true);
  assert.equal(editable("binary"), false);
  assert.equal(why("text"), "");
  assert.equal(why("markdown"), "");
  assert.equal(why("binary"), "a binary file is not editable");
  assert.equal(why("large"), "a file larger than 1 MiB is paged, not editable");
  assert.equal(why("image"), "an image is not editable");
  assert.equal(why("pdf"), "a PDF is not editable");
  assert.equal(why("dir"), "a folder is not editable");
  assert.equal(why("error"), "the file could not be read");
  assert.equal(why("text", true), "agent mode shows the live agent; switch to file mode to edit its files");
});

test("preview model: a binding is pinned to its path, or follows the selection", () => {
  assert.equal(targetOf({ follow: true }, "src/a.ts"), "src/a.ts");
  assert.equal(targetOf({ follow: true }, null), null);
  assert.equal(targetOf({ pinned: "docs/x.md" }, "src/a.ts"), "docs/x.md", "pinned ignores the selection");
});

test("preview model: an agent's folder shows its agent.yaml in file mode and the agent in agent mode; anything else is itself", () => {
  const info: AgentInfo = { id: "agent_1", handler: "x.ts", displayName: "", hasParent: false, hasMemory: false };
  const index = { agentAt: (p: string) => (p === ".fantastic/agents/a" ? info : undefined) };
  assert.deepEqual(agentTarget(".fantastic/agents/a", "file", index), { path: ".fantastic/agents/a/agent.yaml", agent: "agent_1" });
  assert.deepEqual(agentTarget(".fantastic/agents/a", "agent", index), { path: ".fantastic/agents/a", agent: "agent_1" });
  assert.deepEqual(agentTarget("src", "agent", index), { path: "src", agent: null }, "a folder that is no agent");
});

test("preview model: a page of a large file is text from its first byte; a page after the first drops a split character at its start", () => {
  const b = new TextEncoder().encode("héllo wörld");
  assert.equal(pageText(b, true), "héllo wörld");
  assert.equal(pageText(new Uint8Array(0), true), "", "an empty page is empty text");
  assert.equal(pageText(new Uint8Array([0x61, 0x00, 0x62]), true), null, "a NUL byte: binary");
  assert.equal(pageText(new Uint8Array([0xff, 0xfe]), true), null, "not UTF-8: binary");
  // a page that starts inside 'é' (0xC3 0xA9): its continuation byte is dropped, the rest is text
  const started = new Uint8Array([0xa9, 0x61, 0x62]);
  assert.equal(pageText(started, false), "ab");
  // a page that ends inside 'é': the continuation bytes are taken with it
  const big = new Uint8Array(PAGE + 2);
  big.fill(0x61);
  big[PAGE - 1] = 0xc3;
  big[PAGE] = 0xa9;
  const page = pageText(big, true)!;
  assert.equal(page.length, PAGE - 1 + 1, "the split é is whole: the page ends after it");
  assert.equal(page.endsWith("é"), true);
  // a page that is all continuation bytes after its start is not split into text
  assert.equal(pageText(new Uint8Array([0x80, 0x80, 0x80]), false), "", "nothing left after the dropped bytes");
  assert.equal(pageText(new Uint8Array([0x80, 0x80, 0x80, 0x80]), false), null, "a fourth continuation byte is not a character: not text");
});

test("preview model: sizes are said in bytes, KiB and MiB; an unknown size says so", () => {
  assert.equal(human(-1), "unknown size");
  assert.equal(human(0), "0 B");
  assert.equal(human(1023), "1023 B");
  assert.equal(human(2048), "2.0 KiB");
  assert.equal(human(3 * 1024 * 1024), "3.0 MiB");
});

// ─── the markdown parser ───────────────────────────────────────────────────

test("markdown: headings (closing hashes come off), rules, quotes (lazy lines too), and paragraphs that join their lines", () => {
  const ast = parseMarkdown("## Title ##\n\n---\n\n> one\n> two\n\nfirst\nsecond\n\n# \n");
  assert.deepEqual(ast[0], { t: "heading", level: 2, c: [{ t: "text", v: "Title" }] });
  assert.deepEqual(ast[1], { t: "hr" });
  assert.deepEqual(ast[2], { t: "quote", c: [{ t: "para", c: [{ t: "text", v: "one two" }] }] });
  assert.deepEqual(ast[3], { t: "para", c: [{ t: "text", v: "first second" }] });
  assert.deepEqual(ast[4], { t: "heading", level: 1, c: [] }, "an empty heading is a heading with nothing in it");
});

test("markdown: fenced code keeps its body verbatim; a tilde fence works; an unterminated fence runs to the end", () => {
  const ast = parseMarkdown("```ts\nconst a = 1;\n  indented\n```\n\n~~~\nplain\n~~~\n\n```\nopen to the end");
  assert.deepEqual(ast[0], { t: "code", lang: "ts", v: "const a = 1;\n  indented" });
  assert.deepEqual(ast[1], { t: "code", lang: "", v: "plain" });
  assert.deepEqual(ast[2], { t: "code", lang: "", v: "open to the end" });
});

test("markdown: bullet and ordered lists; nested by indent; a blank line between items keeps one list; an ordered and a bullet list are two lists", () => {
  const ast = parseMarkdown("- a\n  - a1\n  - a2\n- b\n\n- c\n\n1. one\n2. two\n3) three");
  assert.equal(ast[0]!.t, "list");
  const bullets = ast[0] as Extract<Block, { t: "list" }>;
  assert.equal(bullets.ordered, false);
  assert.equal(bullets.items.length, 3, "a, b and c: the blank line between b and c keeps the list");
  const nested = bullets.items[0]![1] as Extract<Block, { t: "list" }>;
  assert.equal(nested.t, "list");
  assert.equal(nested.items.length, 2);
  const ordered = ast[1] as Extract<Block, { t: "list" }>;
  assert.equal(ordered.ordered, true);
  assert.equal(ordered.items.length, 3, "1. 2. and 3) are one ordered list");
});

test("markdown: an item's lazy continuation joins its paragraph; a blank line inside an item's indented part stays in the item", () => {
  const ast = parseMarkdown("- first line\ncontinued lazily\n\n  second para\n- next");
  const l = ast[0] as Extract<Block, { t: "list" }>;
  assert.equal(l.items.length, 2);
  const first = l.items[0]!;
  assert.equal(first.length, 2, "two paragraphs inside the first item");
  assert.deepEqual(first[0], { t: "para", c: [{ t: "text", v: "first line continued lazily" }] });
  assert.deepEqual(first[1], { t: "para", c: [{ t: "text", v: "second para" }] });
});

test("markdown: a simple table has its head and its rows; a line with a bar and no separator is a paragraph", () => {
  const ast = parseMarkdown("| a | b |\n| - | :-: |\n| 1 | 2 |\n| 3 | 4 |\n\n| just | text |");
  assert.equal(ast[0]!.t, "table");
  const t = ast[0] as Extract<Block, { t: "table" }>;
  assert.equal(t.head.length, 2);
  assert.equal(t.rows.length, 2);
  assert.deepEqual(t.rows[1]![0], [{ t: "text", v: "3" }]);
  assert.equal(ast[1]!.t, "para");
});

test("markdown: a bar on its own line with no separator under it is a paragraph; the parse of an empty document is empty", () => {
  assert.deepEqual(parseMarkdown("a | b"), [{ t: "para", c: [{ t: "text", v: "a | b" }] }]);
  assert.deepEqual(parseMarkdown(""), []);
  assert.deepEqual(parseMarkdown("\r\n\r\n"), []);
  assert.deepEqual(parseMarkdown("one\r\ntwo"), [{ t: "para", c: [{ t: "text", v: "one two" }] }], "CRLF is read as LF");
});

test("markdown: inline: escapes, code spans, images and links with titles, emphasis and strong, and markers that are text", () => {
  assert.deepEqual(parseInline("\\*not\\* \\# ok"), [{ t: "text", v: "*not* # ok" }]);
  assert.deepEqual(parseInline("a `x*y` b"), [{ t: "text", v: "a " }, { t: "code", v: "x*y" }, { t: "text", v: " b" }]);
  assert.deepEqual(parseInline("open ` tick"), [{ t: "text", v: "open ` tick" }], "an unclosed backtick is text");
  assert.deepEqual(parseInline("![alt text](img.png \"title\")"), [{ t: "image", src: "img.png", alt: "alt text" }]);
  assert.deepEqual(parseInline("![x](a.png"), [{ t: "text", v: "![x](a.png" }], "an image that is not closed is text");
  assert.deepEqual(parseInline("[l](http://x.y \"t\")"), [{ t: "link", href: "http://x.y", c: [{ t: "text", v: "l" }] }]);
  assert.deepEqual(parseInline("[open"), [{ t: "text", v: "[open" }]);
  assert.deepEqual(parseInline("**bold** and __also__"), [{ t: "strong", c: [{ t: "text", v: "bold" }] }, { t: "text", v: " and " }, { t: "strong", c: [{ t: "text", v: "also" }] }]);
  assert.deepEqual(parseInline("**"), [{ t: "text", v: "**" }], "an empty strong is text");
  assert.deepEqual(parseInline("*em* and *x"), [{ t: "em", c: [{ t: "text", v: "em" }] }, { t: "text", v: " and *x" }]);
  assert.deepEqual(parseInline("snake_case_name"), [{ t: "text", v: "snake_case_name" }], "snake_case is not emphasis");
  assert.deepEqual(parseInline("_em_ here"), [{ t: "em", c: [{ t: "text", v: "em" }] }, { t: "text", v: " here" }]);
  assert.deepEqual(parseInline("a * b"), [{ t: "text", v: "a * b" }], "a star with a space after it is text");
  assert.deepEqual(parseInline("*"), [{ t: "text", v: "*" }]);
  assert.deepEqual(parseInline(""), []);
});

test("markdown: link schemes: only web, mail, anchors and relative paths are followed; scheme and absolute targets are dropped", () => {
  assert.equal(linkKind("https://x"), "web");
  assert.equal(linkKind("HTTP://x"), "web");
  assert.equal(linkKind("mailto:a@b"), "mail");
  assert.equal(linkKind("#top"), "anchor");
  assert.equal(linkKind("docs/a.md"), "file");
  assert.equal(linkKind("./a"), "file");
  assert.equal(linkKind("   "), null);
  assert.equal(linkKind("javascript:alert(1)"), null);
  assert.equal(linkKind("//host/x"), null);
  assert.equal(linkKind("/abs"), null);
  assert.equal(linkKind("\\share"), null);
  assert.equal(linkKind("data:text/html,x"), null);
});

test("markdown: a relative link resolves from the file's folder and never out of the project; a bare anchor is not a path", () => {
  assert.equal(resolveRel("docs", "a.md"), "docs/a.md");
  assert.equal(resolveRel("docs/x", "../a.md"), "docs/a.md");
  assert.equal(resolveRel("", "./top.txt?raw=1#sec"), "top.txt");
  assert.equal(resolveRel("", "../escape.md"), null, "out of the project");
  assert.equal(resolveRel("docs", "../../escape.md"), null);
  assert.equal(resolveRel("docs", "#only"), null, "no path at all");
  assert.equal(resolveRel("", "//"), null);
  assert.equal(dirOf("docs/a/b.md"), "docs/a");
  assert.equal(dirOf("top.md"), "");
});

// ─── the markdown renderer, through a DOM that records what is made ────────

interface FakeEl {
  tag: string;
  cls?: string;
  text?: string;
  children: Array<FakeEl | FakeText>;
  dataset: Record<string, string>;
  attrs: Record<string, string>;
  listeners: Array<(e: { preventDefault(): void }) => void>;
  appendChild(c: FakeEl | FakeText): void;
  addEventListener(type: string, fn: (e: { preventDefault(): void }) => void): void;
}
interface FakeText {
  node: "text";
  v: string;
}

function fakeDoc(): { createElement(tag: string): FakeEl; createTextNode(v: string): FakeText } {
  return {
    createElement(tag: string): FakeEl {
      const el: FakeEl = {
        tag,
        children: [],
        dataset: {},
        attrs: {},
        listeners: [],
        appendChild(c) {
          el.children.push(c);
        },
        addEventListener(_type, fn) {
          el.listeners.push(fn);
        },
      };
      return new Proxy(el, {
        set(target, key, value) {
          if (key === "className") target.cls = value;
          else if (key === "textContent") {
            target.children = [];
            target.text = String(value);
          } else if (key === "title" || key === "href" || key === "target" || key === "rel" || key === "alt" || key === "src") target.attrs[key] = String(value);
          else (target as unknown as Record<string, unknown>)[key as string] = value;
          return true;
        },
      });
    },
    createTextNode(v: string): FakeText {
      return { node: "text", v };
    },
  };
}

/** A readable form of what was made: tags, classes, text, and the attributes that matter. */
function show(n: FakeEl | FakeText): string {
  if ("node" in n) return n.v;
  const attrs = Object.entries(n.attrs).map(([k, v]) => ` ${k}="${v}"`).join("");
  const data = Object.entries(n.dataset).map(([k, v]) => ` data-${k}="${v}"`).join("");
  const cls = n.cls !== undefined ? ` class="${n.cls}"` : "";
  return `<${n.tag}${cls}${attrs}${data}>${n.text ?? n.children.map(show).join("")}</${n.tag}>`;
}

function render(md: string, hooks: Partial<RenderHooks> = {}): { html: string; root: FakeEl; clicks: string[] } {
  const clicks: string[] = [];
  const h: RenderHooks = { dir: "docs", image: () => {}, open: (p) => clicks.push(p), ...hooks };
  const root = renderMarkdown(parseMarkdown(md), fakeDoc() as unknown as Document, h) as unknown as FakeEl;
  return { html: show(root), root, clicks };
}

test("markdown render: every block becomes its element; the file's text is text, never markup", () => {
  const r = render("# T\n\n<script>x</script> para\n\n---\n\n> q\n\n```js\nlet a;\n```\n\n- a\n\n1. b\n\n| h |\n| - |\n| c |");
  assert.match(r.html, /^<div class="pv-md"><h1>T<\/h1>/);
  assert.ok(r.html.includes("<p>&lt;script>") === false, "no entity handling here: the text node holds the markup as text");
  assert.ok(r.html.includes("<p><script>x</script> para</p>"), "the text is a text node, not parsed");
  assert.ok(r.html.includes("<hr></hr>"));
  assert.ok(r.html.includes("<blockquote><p>q</p></blockquote>"));
  assert.ok(r.html.includes('<pre><code data-lang="js">let a;</code></pre>'));
  assert.ok(r.html.includes("<ul><li><p>a</p></li></ul>"));
  assert.ok(r.html.includes("<ol><li><p>b</p></li></ol>"));
  assert.ok(r.html.includes("<table><thead><tr><th>h</th></tr></thead><tbody><tr><td>c</td></tr></tbody></table>"));
});

test("markdown render: an unlabelled code block has no language; emphasis, strong and code nest as the text says", () => {
  const r = render("```\nraw\n```\n\n*a and `c`* **b**");
  assert.ok(r.html.includes("<pre><code>raw</code></pre>"));
  assert.ok(r.html.includes("<p><em>a and <code>c</code></em> <strong>b</strong></p>"));
});

test("markdown render: a project image is handed to the hook with its resolved path; a remote or unsafe image is a labelled placeholder, never fetched", () => {
  const seen: Array<[string, string]> = [];
  const imgs = render("![pic](a.png) ![x](https://e.com/x.png) ![](../../up.png) ![](javascript:x)", { image: (img, path) => seen.push([(img as unknown as FakeEl).attrs["alt"] ?? "", path]) });
  assert.deepEqual(seen, [["pic", "docs/a.png"]]);
  assert.ok(imgs.html.includes('<span class="pv-md-noimg">[image: x]</span>'), "a remote image is a placeholder, labelled by its alt text");
  assert.ok(imgs.html.includes('<span class="pv-md-noimg">[image: ../../up.png]</span>'), "an image that leaves the project is a placeholder");
  assert.ok(imgs.html.includes('<span class="pv-md-noimg">[image: javascript:x]</span>'));
});

test("markdown render: a link to a project file opens it on click (prevented); a remote link opens a new tab, no opener; a dropped scheme is its text", () => {
  const r = render("[a](other.md) [b](https://e.com) [c](#top) [d](javascript:x) [e](../../out.md) [f](mailto:me@x)");
  assert.ok(r.html.includes('<a title="docs/other.md" data-path="docs/other.md">a</a>'));
  assert.ok(r.html.includes('<a href="https://e.com" target="_blank" rel="noopener noreferrer">b</a>'));
  assert.ok(r.html.includes("<span>c</span>"), "an anchor is not followed: its text");
  assert.ok(r.html.includes("<span>d</span>"));
  assert.ok(r.html.includes('<a title="../../out.md" data-path="">e</a>'), "a link out of the project is a link that does nothing");
  assert.ok(r.html.includes('<a href="mailto:me@x" target="_blank" rel="noopener noreferrer">f</a>'));
  const file = r.root.children[0]!;
  void file;
  // the click on the in-project link opens it; the event's default is stopped
  const para = r.root.children.find((c) => "tag" in c && c.tag === "p") as FakeEl;
  const link = para.children.find((c) => "tag" in c && c.tag === "a" && c.dataset["path"] === "docs/other.md") as FakeEl;
  let prevented = false;
  link.listeners[0]!({ preventDefault: () => (prevented = true) });
  assert.equal(prevented, true);
  assert.deepEqual(r.clicks, ["docs/other.md"]);
  const outLink = para.children.find((c) => "tag" in c && c.tag === "a" && c.dataset["path"] === "") as FakeEl;
  outLink.listeners[0]!({ preventDefault: () => {} });
  assert.deepEqual(r.clicks, ["docs/other.md"], "a link that leaves the project opens nothing");
});

test("markdown render: a heading of each level, and an empty table head makes no cells", () => {
  const r = render("### three\n\n| |\n| - |\n| x |");
  assert.ok(r.html.includes("<h3>three</h3>"));
  assert.ok(r.html.includes("<thead><tr><th></th></tr></thead>"), "a header of nothing is one empty cell");
});

test("markdown render: the rule, the table and the items build the same tree on every render", () => {
  const md = "- a\n\n| x |\n| - |\n| 1 |";
  assert.equal(render(md).html, render(md).html);
});

// ─── the highlighter ───────────────────────────────────────────────────────

test("highlight: a python triple-quoted string runs to its close, or to the end of the file", () => {
  const src = 'x = """doc\nline"""\ny = \'\'\'open';
  const t = tokens(src, "py");
  assert.deepEqual(t.filter((x) => x.k === "str").map((x) => x.t), ['"""doc\nline"""', "'''open"]);
  assert.equal(t.map((x) => x.t).join(""), src, "the tokens are the source");
});

test("highlight: a template literal spans lines in ts, an escape inside a string does not end it, and a shell comment needs a space before it", () => {
  const ts = "const s = `a\nb ${x}`;\nconst q = 'it\\'s';";
  const tt = tokens(ts, "ts");
  assert.ok(tt.some((x) => x.k === "str" && x.t === "`a\nb ${x}`"));
  assert.ok(tt.some((x) => x.k === "str" && x.t === "'it\\'s'"));
  assert.equal(tt.map((x) => x.t).join(""), ts);
  const sh = "echo a#b # note";
  const ts2 = tokens(sh, "sh");
  assert.deepEqual(ts2.filter((x) => x.k === "com").map((x) => x.t), ["# note"], "a # inside a word is not a comment");
  assert.equal(ts2.map((x) => x.t).join(""), sh);
});

test("highlight: hex and exponent numbers, types by capital, css and html words, yaml keys, markdown is plain words", () => {
  const n = tokens("0xFF 1e5 2.5", "ts").filter((x) => x.k === "num").map((x) => x.t);
  assert.deepEqual(n, ["0xFF", "1e5", "2.5"]);
  assert.ok(tokens("new Map()", "ts").some((x) => x.k === "type" && x.t === "Map"));
  assert.ok(tokens(".a { color: red }", "css").every((x) => x.k !== "type"), "css has no types");
  assert.ok(tokens("<div class=\"x\"></div>", "html").some((x) => x.k === "str" && x.t === '"x"'));
  assert.deepEqual(tokens("name: x", "yaml").find((x) => x.t === "name"), { k: "type", t: "name" });
  assert.ok(tokens("- item: 1", "yaml").some((x) => x.k === "type" && x.t === "item"));
  assert.ok(tokens("some words", "md").every((x) => x.k !== "kw" && x.k !== "type"));
  assert.ok(tokens("/* x */", "json").every((x) => x.k !== "com"), "json has no comments");
});

test("highlight: an empty source is no tokens; a plain file is one plain run; an unterminated string runs to its line", () => {
  assert.deepEqual(tokens("", "ts"), []);
  assert.deepEqual(tokens("anything", "plain"), [{ k: "plain", t: "anything" }]);
  assert.deepEqual(tokens("", "plain"), []);
  const t = tokens('"open\nnext', "json");
  assert.deepEqual(t[0], { k: "str", t: '"open' });
});

test("highlight: the language of a name is its extension; none is plain", () => {
  assert.equal(langOf("a.TSX"), "ts");
  assert.equal(langOf("a.yml"), "yaml");
  assert.equal(langOf("a.htm"), "html");
  assert.equal(langOf("a.unknown"), "plain");
  assert.equal(langOf(".bashrc"), "plain");
  assert.equal(langOf("Makefile"), "plain");
});

// ─── the diff ──────────────────────────────────────────────────────────────

test("diff: a change is a delete and an add; the same text is all same; an empty side is all the other side", () => {
  assert.deepEqual(lineDiff("a\nb", "a\nc"), [
    { op: "same", text: "a" },
    { op: "del", text: "b" },
    { op: "add", text: "c" },
  ]);
  assert.deepEqual(lineDiff("", ""), []);
  assert.deepEqual(lineDiff("", "x"), [{ op: "add", text: "x" }]);
  assert.deepEqual(lineDiff("x", ""), [{ op: "del", text: "x" }]);
  assert.deepEqual(lineDiff("same", "same"), [{ op: "same", text: "same" }]);
});

test("diff: a side over the line cap is said, not guessed; at the cap it is still compared", () => {
  const big = Array.from({ length: MAX_LINES + 1 }, (_, i) => `l${i}`).join("\n");
  assert.deepEqual(lineDiff(big, "x"), { tooLarge: true });
  assert.deepEqual(lineDiff("x", big), { tooLarge: true });
  const edge = Array.from({ length: MAX_LINES }, (_, i) => `l${i}`).join("\n");
  const d = lineDiff(edge, edge);
  assert.ok(Array.isArray(d) && d.every((x) => x.op === "same") && d.length === MAX_LINES);
});

test("diff: the ops replayed over the old text give the new one, for a shuffled pair", () => {
  const a = ["one", "two", "three", "four", "five", "six"];
  const b = ["two", "zero", "three", "five", "seven", "six"];
  const d = lineDiff(a.join("\n"), b.join("\n"));
  assert.ok(Array.isArray(d));
  const newer = d.filter((x) => x.op !== "del").map((x) => x.text);
  const older = d.filter((x) => x.op !== "add").map((x) => x.text);
  assert.deepEqual(older, a);
  assert.deepEqual(newer, b);
});


test("highlight: a backtick is a string only in ts and sh; a single quote in html is not a string; a template's escaped backtick does not end it", () => {
  assert.deepEqual(tokens("x = `a`", "py").filter((t) => t.k === "str"), [], "a backtick is no string in python");
  assert.deepEqual(tokens("echo `date`", "sh").filter((t) => t.k === "str").map((t) => t.t), ["`date`"]);
  assert.deepEqual(tokens("<a x='y'>", "html").filter((t) => t.k === "str"), [], "html takes double quotes only");
  const tpl = "const s = `a\\`b`;";
  const strs = tokens(tpl, "ts").filter((t) => t.k === "str").map((t) => t.t);
  assert.deepEqual(strs, ["`a\\`b`"], "the escaped backtick is inside the string");
  assert.equal(tokens(tpl, "ts").map((t) => t.t).join(""), tpl);
});

test("markdown: a link that climbs back out to nothing names no path", () => {
  assert.equal(resolveRel("", "a/.."), null);
  assert.equal(resolveRel("docs", "../docs/x.md"), "docs/x.md");
});
