// A BODY OF TEXT from a call. ai_core's convention sends a body verbatim under
// `bytes`, and across the bridge `bytes` rides a BINARY frame — so it arrives as
// raw bytes (a Uint8Array / ArrayBuffer), not a string. Decoded as UTF-8 here;
// a string passes through; anything else (a parsed object — the grammar read
// markup's tags as arguments) is not text: null.
export function bodyText(raw: unknown): string | null {
  if (typeof raw === "string") return raw;
  if (raw instanceof ArrayBuffer) return new TextDecoder().decode(new Uint8Array(raw));
  if (ArrayBuffer.isView(raw)) return new TextDecoder().decode(new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength));
  return null;
}
