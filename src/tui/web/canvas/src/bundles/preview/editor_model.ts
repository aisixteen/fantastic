// THE EDITOR'S STATE and the conflict it can meet. Pure; the controller runs
// the effects. The rules: a save always carries `if_match` (the version the
// edits are based on); a change on disk under unsaved edits is a CONFLICT that
// the person resolves; nothing is ever merged, and nothing is ever written
// without the version it replaces.

export type EditorState =
  | { k: "viewing"; text: string; version: string }
  | { k: "dirty"; base: string; baseVersion: string; mine: string }
  | { k: "saving"; base: string; baseVersion: string; mine: string; sent: string; awaitingTheirs: boolean }
  | { k: "conflict"; base: string; baseVersion: string; mine: string; theirs: string; theirsVersion: string }
  | { k: "comparing"; base: string; baseVersion: string; mine: string; theirs: string; theirsVersion: string };

export type EditorEffect = { kind: "write"; bytes: string; ifMatch: string } | { kind: "read" } | { kind: "undo" };

export type Revert = { ok: true; what: "edits" | "save"; effects: EditorEffect[] } | { ok: false; reason: "nothing_to_revert" };

export class EditorModel {
  state: EditorState = { k: "viewing", text: "", version: "" };
  /** the version our own last save wrote: its echo from the watch is no conflict */
  lastSaved: string | null = null;
  /** a refusal of the last save, for the editor to show */
  error: string | null = null;

  /** A file loaded fresh (a new path): nothing is remembered. */
  load(text: string, version: string): void {
    this.state = { k: "viewing", text, version };
    this.lastSaved = null;
    this.error = null;
  }

  /** The text on screen. */
  text(): string {
    const s = this.state;
    return s.k === "viewing" ? s.text : s.mine;
  }

  /** The version the shown text is based on. */
  version(): string {
    const s = this.state;
    return s.k === "viewing" ? s.version : s.baseVersion;
  }

  isDirty(): boolean {
    const s = this.state;
    return s.k !== "viewing" && s.mine !== s.base;
  }

  inConflict(): boolean {
    return this.state.k === "conflict" || this.state.k === "comparing";
  }

  /** The person typed. Ignored while a save is in flight or a conflict is open (the editor is read-only then). */
  edit(text: string): void {
    const s = this.state;
    if (s.k === "viewing") {
      if (text !== s.text) this.state = { k: "dirty", base: s.text, baseVersion: s.version, mine: text };
    } else if (s.k === "dirty") {
      s.mine = text;
    }
  }

  save(): EditorEffect[] {
    const s = this.state;
    if (s.k !== "dirty") return [];
    this.error = null;
    this.state = { k: "saving", base: s.base, baseVersion: s.baseVersion, mine: s.mine, sent: s.mine, awaitingTheirs: false };
    return [{ kind: "write", bytes: s.mine, ifMatch: s.baseVersion }];
  }

  saved(version: string): void {
    const s = this.state;
    if (s.k !== "saving") return;
    this.lastSaved = version;
    this.state = s.mine === s.sent ? { k: "viewing", text: s.sent, version } : { k: "dirty", base: s.sent, baseVersion: version, mine: s.mine };
  }

  /** The save was refused. `version_mismatch`: read what is there, then conflict; any other: back to dirty, said. */
  saveRefused(r: { reason?: unknown; error?: unknown }): EditorEffect[] {
    const s = this.state;
    if (s.k !== "saving") return [];
    if (r.reason === "version_mismatch") {
      s.awaitingTheirs = true;
      return [{ kind: "read" }];
    }
    this.error = String(r.error ?? "the save was refused");
    this.state = { k: "dirty", base: s.base, baseVersion: s.baseVersion, mine: s.mine };
    return [];
  }

  /** The disk reported a change: read it again. */
  diskChanged(): EditorEffect[] {
    return [{ kind: "read" }];
  }

  /** The file as read after a change. */
  reread(r: { text: string; version: string }): void {
    const s = this.state;
    switch (s.k) {
      case "viewing":
        if (r.version !== s.version) this.state = { k: "viewing", text: r.text, version: r.version };
        return;
      case "dirty":
        if (r.version === s.baseVersion || r.version === this.lastSaved) return; // ours: nothing new
        this.state = { k: "conflict", base: s.base, baseVersion: s.baseVersion, mine: s.mine, theirs: r.text, theirsVersion: r.version };
        return;
      case "saving":
        if (s.awaitingTheirs) this.state = { k: "conflict", base: s.base, baseVersion: s.baseVersion, mine: s.mine, theirs: r.text, theirsVersion: r.version };
        return; // else the write's own answer decides
      case "conflict":
      case "comparing":
        if (r.version !== s.theirsVersion) this.state = { ...s, theirs: r.text, theirsVersion: r.version };
        return;
    }
  }

  /** Conflict: drop mine, take the disk's. */
  reload(): void {
    const s = this.state;
    if (s.k !== "conflict" && s.k !== "comparing") return;
    this.state = { k: "viewing", text: s.theirs, version: s.theirsVersion };
  }

  /** Conflict: overwrite — against the version just seen, so a third writer is a fresh conflict, never a blind write. */
  keepMine(): EditorEffect[] {
    const s = this.state;
    if (s.k !== "conflict" && s.k !== "comparing") return [];
    this.error = null;
    this.state = { k: "saving", base: s.theirs, baseVersion: s.theirsVersion, mine: s.mine, sent: s.mine, awaitingTheirs: false };
    return [{ kind: "write", bytes: s.mine, ifMatch: s.theirsVersion }];
  }

  compare(): void {
    const s = this.state;
    if (s.k === "conflict") this.state = { ...s, k: "comparing" };
  }

  revert(): Revert {
    const s = this.state;
    if (s.k === "dirty") {
      this.error = null;
      this.state = { k: "viewing", text: s.base, version: s.baseVersion };
      return { ok: true, what: "edits", effects: [] };
    }
    if (s.k === "viewing" && this.lastSaved !== null && s.version === this.lastSaved) {
      return { ok: true, what: "save", effects: [{ kind: "undo" }, { kind: "read" }] };
    }
    return { ok: false, reason: "nothing_to_revert" };
  }
}
