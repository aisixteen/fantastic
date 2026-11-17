// view_skins.ts — write the html views' skin blocks (`src/bundles/skin/view_skins.ts`) to
// `web/page/view_skins/<name>.css`, for a host that does not compile tokens (a product page
// served from Rust: `fantastic_tui::VIEW_SKINS`). `tests/view_skins.test.ts` holds the files
// to what this writes.
//
//   node scripts/view_skins.ts
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { VIEW_SKINS, viewSkinBlock } from "../src/bundles/skin/view_skins.ts";

const DIR = fileURLToPath(new URL("../../page/view_skins/", import.meta.url));
for (const name of VIEW_SKINS) writeFileSync(`${DIR}${name}.css`, viewSkinBlock(name));
