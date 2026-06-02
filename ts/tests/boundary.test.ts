import { test } from "node:test";
import assert from "node:assert/strict";
import { readdir, readFile } from "node:fs/promises";

// Purity boundary, headless half: the pure core (src/kernel/) must not import
// any view bundle or vendored lib. The OTHER half — "no DOM type/global" — is
// enforced at compile time by src/kernel/tsconfig.json (lib without `DOM`),
// run via `npm run check:pure`; doing it here by grep would false-positive on
// the prose in these very comments.

const kernelDir = new URL("../src/kernel/", import.meta.url);

function importSpecifiers(source: string): string[] {
  const out: string[] = [];
  const fromRe = /(?:^|\s)(?:import|export)\b[^;\n]*?\bfrom\s*["']([^"']+)["']/g;
  const dynRe = /\bimport\s*\(\s*["']([^"']+)["']\s*\)/g;
  for (const m of source.matchAll(fromRe)) if (m[1]) out.push(m[1]);
  for (const m of source.matchAll(dynRe)) if (m[1]) out.push(m[1]);
  return out;
}

test("src/kernel/ imports nothing from bundles/ or vendor/", async () => {
  const files = (await readdir(kernelDir)).filter((f) => f.endsWith(".ts"));
  assert.ok(files.length >= 4, "expected the core .ts files to exist");
  for (const file of files) {
    const src = await readFile(new URL(file, kernelDir), "utf8");
    for (const spec of importSpecifiers(src)) {
      assert.ok(
        !/(^|\/)(bundles|vendor)\//.test(spec) &&
          !spec.includes("../bundles") &&
          !spec.includes("../vendor"),
        `${file} imports a view concern: ${spec}`,
      );
      assert.ok(
        spec.startsWith("node:") || spec.startsWith("./"),
        `${file} import '${spec}' should be a node: builtin or a kernel-local ./path`,
      );
    }
  }
});
