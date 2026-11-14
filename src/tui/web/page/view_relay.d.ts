export type Calls = Record<string, string[]>;
export interface ViewTransport {
  send(target: string, payload: Record<string, unknown>): Promise<unknown> | unknown;
  watch(src: string, deliver: (payload: unknown) => void): Promise<{ off: () => void } | { denied: unknown }> | { off: () => void } | { denied: unknown };
  emit?(target: string, payload: Record<string, unknown>): unknown;
}
export interface ViewOptions {
  mount: { appendChild(n: unknown): unknown };
  html: string | (() => string | Promise<string>);
  transport: ViewTransport;
  onMessage?: (m: any, raw?: any) => void;
  calls?: Calls;
  cell?: string;
  keep?: { load(): unknown; save(state: unknown): void };
  extra?: string;
  trusted?: boolean;
  inbox?: (deliver: (payload: unknown) => void) => () => void;
  win?: any;
  doc?: any;
}
export interface ViewHandle {
  iframe: any;
  reload(): void;
  inbox(payload: unknown): void;
  unmount(): void;
}
export function connectorSrc(extra?: string): string;
export function callAllowed(calls: Calls, target: string, verb: string): boolean;
export function mountView(opts: ViewOptions): ViewHandle;
