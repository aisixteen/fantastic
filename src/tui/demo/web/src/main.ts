// The entry: mount the demo on `#demo` with the sections in order.

import { mount } from "./app.ts";
import { SECTIONS } from "./sections/index.ts";

const root = document.getElementById("demo");
if (root) mount(root, SECTIONS);
