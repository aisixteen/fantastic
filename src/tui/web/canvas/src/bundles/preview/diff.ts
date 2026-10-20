// A LINE DIFF by longest common subsequence — for the conflict's "compare".
// Pure. Past `MAX_LINES` a side it says so instead of guessing.

export interface DiffLine {
  op: "same" | "del" | "add";
  text: string;
}

export const MAX_LINES = 4000;

export function lineDiff(a: string, b: string): DiffLine[] | { tooLarge: true } {
  const x = a === "" ? [] : a.split("\n");
  const y = b === "" ? [] : b.split("\n");
  if (x.length > MAX_LINES || y.length > MAX_LINES) return { tooLarge: true };
  // the common head and tail need no table
  let lo = 0;
  while (lo < x.length && lo < y.length && x[lo] === y[lo]) lo++;
  let ex = x.length;
  let ey = y.length;
  while (ex > lo && ey > lo && x[ex - 1] === y[ey - 1]) {
    ex--;
    ey--;
  }
  const out: DiffLine[] = x.slice(0, lo).map((text) => ({ op: "same", text }));
  const n = ex - lo;
  const m = ey - lo;
  // lcs[i][j]: the common length of x[lo+i..) and y[lo+j..)
  const w = m + 1;
  const lcs = new Uint16Array((n + 1) * w);
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i * w + j] = x[lo + i] === y[lo + j] ? lcs[(i + 1) * w + j + 1]! + 1 : Math.max(lcs[(i + 1) * w + j]!, lcs[i * w + j + 1]!);
    }
  }
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (x[lo + i] === y[lo + j]) {
      out.push({ op: "same", text: x[lo + i]! });
      i++;
      j++;
    } else if (lcs[(i + 1) * w + j]! >= lcs[i * w + j + 1]!) {
      out.push({ op: "del", text: x[lo + i++]! });
    } else {
      out.push({ op: "add", text: y[lo + j++]! });
    }
  }
  while (i < n) out.push({ op: "del", text: x[lo + i++]! });
  while (j < m) out.push({ op: "add", text: y[lo + j++]! });
  for (const text of x.slice(ex)) out.push({ op: "same", text });
  return out;
}
