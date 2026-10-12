// THE SECTIONS, IN ORDER. Adding, removing or reordering a section is editing
// this list; each section is its own module of data.

import type { Section } from "../model.ts";
import { aiContext } from "./ai_context.ts";
import { canvas } from "./canvas.ts";
import { cli } from "./cli.ts";
import { kernel } from "./kernel.ts";

export const SECTIONS: Section[] = [kernel, aiContext, cli, canvas];
