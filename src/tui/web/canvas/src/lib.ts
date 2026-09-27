// THE ONE SEAM between this app and the ts kernel library: the library's public
// entry, re-exported. Every module here imports the kernel API from this file,
// never from the library's internals — the library can move its insides without
// this app noticing, and a reader sees in one place everything the app uses.
export * from "../../../../lib/ts/src/index.ts";
