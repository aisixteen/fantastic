// Ambient module declarations for the vendored libs — so tsc resolves the bare
// specifiers without @types packages (no npm at runtime OR build for these).
// Bodyless `declare module` types the import as `any`; that's the deliberate
// trade for staying hermetic (we vendor the runtime, not the typings). The
// browser resolves these specifiers via the import map in the mount page
// (three/@xterm/* → /…/vendor/*.js). See VENDOR.md for provenance + checksums.

declare module "three";
declare module "@xterm/xterm";
declare module "@xterm/addon-fit";
