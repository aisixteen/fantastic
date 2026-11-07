// THE SIZE A SYSTEM AGENT'S CELL OPENS AT: the network list is wider and taller
// than the rest. Ejected from the base canvas's action popup, which now knows no
// system agent; the popup tile and the `place` call that used it have no hook in
// the base yet (see README, "Open hooks").
export function systemTileSize(id: string): { width: number; height: number } {
  return id === "network" ? { width: 640, height: 480 } : { width: 380, height: 420 };
}
