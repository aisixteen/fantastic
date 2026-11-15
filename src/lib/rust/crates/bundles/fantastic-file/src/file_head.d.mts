export function extOf(name: string): string;
export function human(n: number): string;
export function crumbs(path: string | null): string[];
export function sizeText(n: number): string;
export function lineCount(text: string): number;
export function linesText(n: number): string;
export function metaOf(o: { name: string; size?: number; text?: string; lines?: number }): string;
export interface AgentLookup {
  agentAt(path: string): { id: string; displayName: string; parentId?: string } | undefined;
  folderOf(id: string): string | undefined;
}
export function agentChain(id: string, index: AgentLookup): string[];
export function trail(path: string | null, index: AgentLookup & { paths(): string[] }): string[];
