// FOLDER BUNDLES — `<web root>/bundles/<name>/bundle.yaml` (owner, 2026-10-08):
// code that EXTENDS a bundled agent (`base`) of THIS kernel. The kernel reads them
// through its loader — the host loader's `read_bundles` hands each `bundle.yaml`
// RAW and this kernel parses it with its own reader (`yaml.ts`) — at boot, before
// the tree loads (so an instance's handler resolves), and on the root's
// `reload_bundle`. A base is a bundle the app registered with a `folderBundle`
// builder (`registerBundle(…, {folderBundle})`, e.g. the canvas's html agent);
// it builds the folder's handler and verb docs.
//
// Python `kernel_state.load_bundle`; rust `folders.rs`; swift `Folders.swift`.

import type { Handler, VerbDocs } from "./agent.ts";
import type { Json, Payload } from "./json.ts";
import type { Kernel } from "./kernel.ts";
import { pyRepr } from "./pyjson.ts";
import { parseYaml } from "./yaml.ts";

/** A folder's manifest. */
export const BUNDLE_FILE = "bundle.yaml";

/** What a base builds for a folder that extends it. */
export interface FolderBuilt {
  handler: Handler;
  /** verb -> doc, what `docs` serves (the folder's `bundle.yaml` is the one truth). */
  verbs: VerbDocs;
  /** The bundle-level readme `reflect readme=true` serves for its instances. */
  readme?: string;
}

/** A base's builder: the folder's bundle, or the refusal text naming the field.
 *  `folder` is the folder as its loader names it (`web/bundles/<name>`), `spec`
 *  its parsed `bundle.yaml`. */
export type FolderBase = (name: string, folder: string, spec: Payload) => FolderBuilt | string;

/** One folder as the loader hands it (`read_bundles`): its raw `bundle.yaml`, or
 *  why there is none. */
export interface FolderText {
  name: string;
  folder: string;
  text?: string;
  error?: string;
}

/** What a loader's `read_bundles` answers — `{bundles}`, or a refusal. */
export type ReadFolders = (name?: string) => Promise<Json>;

/** A folder name is its instances' handler_module: letters, digits, `_`, `-`,
 *  starting with a letter or `_`. Python `_FOLDER_NAME`. */
export function isFolderName(name: string): boolean {
  return /^[A-Za-z_][A-Za-z0-9_-]*$/.test(name);
}

/** Load ONE folder into `k`'s registry → null, or the error — recorded on its
 *  catalog row too, never loaded (a failed RELOAD keeps the earlier load). */
export function loadBundle(k: Kernel, entry: FolderText): string | null {
  const { name, folder } = entry;
  const row: Payload = { name, handler_module: name, folder };
  const refuse = (why: string): string => {
    const err = `bundles/${name}: ${why}`;
    k.refuseFolder(name, { ...row, error: err });
    return err;
  };
  if (!isFolderName(name)) {
    return refuse(
      "a bundle folder is named with letters, digits, _ and - (the name is its instances' handler_module)",
    );
  }
  if (k.clashes(name)) return refuse(`${pyRepr(name)} is a built-in bundle — a folder may not shadow it`);
  if (typeof entry.text !== "string") return refuse("no bundle.yaml");
  let spec: unknown;
  try {
    spec = parseYaml(entry.text, `${folder}/${BUNDLE_FILE}`);
  } catch (e) {
    return refuse(`bundle.yaml does not parse: ${e instanceof Error ? e.message : String(e)}`);
  }
  if (spec === null || typeof spec !== "object" || Array.isArray(spec)) {
    return refuse("bundle.yaml is not a mapping");
  }
  const s = spec as Payload;
  const known = k.folderBases().join(", ") || "none";
  const base = s["base"];
  if (typeof base !== "string" || base === "") {
    return refuse(`base: required — the bundled agent it extends (here: ${known})`);
  }
  const sentence = s["sentence"];
  if (typeof sentence !== "string" || sentence.trim() === "") {
    return refuse("sentence: required — one line saying what it is");
  }
  const build = k.folderBase(base);
  if (build === undefined) {
    return refuse(`base: ${pyRepr(base)} is no bundled agent a folder can extend in this kernel — known: ${known}`);
  }
  const built = build(name, folder, s);
  if (typeof built === "string") return refuse(built);
  k.registerFolder(name, built, { ...row, base, sentence: sentence.trim() });
  return null;
}

/** Load every folder a loader's `read_bundles` reply names — the cold read at
 *  boot. A refused read FAILS LOUD: the kernel cannot know its own bundles. */
export function loadBundles(k: Kernel, reply: Json): void {
  const bundles = (reply as Payload | null)?.["bundles"];
  if (!Array.isArray(bundles)) {
    throw new Error(`folder bundles: the loader's read_bundles answered ${JSON.stringify(reply)}`);
  }
  for (const e of bundles) loadBundle(k, e as unknown as FolderText);
}
