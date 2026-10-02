import type { Section } from "../model.ts";

export const cli: Section = {
  id: "cli",
  title: "The terminal UI and the CLI",
  blurb: "The kernel in-process; coding CLIs join over MCP",
  scenes: [
    {
      id: "tui",
      title: "fantastic in a directory",
      nodes: [
        { id: "you", label: "you", kind: "human", wide: { x: 90, y: 200 }, tall: { x: 280, y: 46 } },
        { id: "tui", label: "terminal UI", sub: "fantastic", kind: "ui", wide: { x: 330, y: 200 }, tall: { x: 280, y: 152 } },
        { id: "kernel", label: "kernel", sub: "in the same process", kind: "kernel", wide: { x: 620, y: 200 }, tall: { x: 280, y: 289 } },
        { id: "dir", label: ".fantastic/", sub: "records · memory · docs", kind: "store", w: 180, wide: { x: 880, y: 200 }, tall: { x: 110, y: 426 } },
        { id: "coder", label: "coding CLI", sub: "Claude Code · Codex · …", kind: "ext", w: 190, wide: { x: 330, y: 430 }, tall: { x: 440, y: 684 } },
        { id: "mcp", label: "fantastic mcp", sub: "reflect · send", kind: "agent", wide: { x: 620, y: 430 }, tall: { x: 440, y: 562 } },
        { id: "sock", label: "mcp.sock", sub: ".fantastic/cli/", kind: "agent", w: 150, wide: { x: 880, y: 430 }, tall: { x: 440, y: 426 } },
      ],
      edges: [
        { id: "e_keys", from: "you", to: "tui" },
        { id: "e_send", from: "tui", to: "kernel" },
        { id: "e_disk", from: "kernel", to: "dir" },
        { id: "e_mcp", from: "coder", to: "mcp" },
        { id: "e_sock", from: "mcp", to: "sock" },
        { id: "e_att", from: "sock", to: "kernel" },
      ],
      steps: [
        {
          caption: "Run fantastic in a directory: the terminal UI starts the kernel inside its own process, and a fresh directory is seeded with .fantastic/.",
          show: ["tui", "kernel", "dir", "e_send", "e_disk"],
          flow: [{ edge: "e_disk", label: "seed" }],
        },
        {
          caption: "The sidebar is the agent tree, the page is the selected agent, and the composer sends to it. t opens a terminal.",
          show: ["you", "e_keys"],
          focus: ["you", "tui"],
          flow: [{ edge: "e_keys", label: "keys" }, { edge: "e_send", label: "send" }],
        },
        {
          caption: "Everything lives in .fantastic/: the records, the readmes and the memory store. The next run rehydrates it.",
          flow: [
            { edge: "e_disk", label: "records · rows" },
            { edge: "e_disk", label: "rehydrate", back: true },
          ],
        },
        {
          caption: "A coding CLI joins through fantastic mcp, with two tools: reflect and send.",
          show: ["coder", "mcp", "e_mcp"],
          flow: [{ edge: "e_mcp", label: "reflect" }],
        },
        {
          caption: "With a kernel already running, fantastic mcp attaches to it over .fantastic/cli/mcp.sock instead of starting its own.",
          show: ["sock", "e_sock", "e_att"],
          flow: [{ edge: "e_sock", label: "send" }, { edge: "e_att", label: "send" }],
        },
        {
          caption: "Run without a terminal, fantastic only prints its readme and exits, so an agent can learn what it is.",
          focus: ["coder"],
        },
      ],
    },
  ],
};
