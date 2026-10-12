import type { Section } from "../model.ts";

export const kernel: Section = {
  id: "kernel",
  title: "The kernel and its agents",
  blurb: "One tree, one verb, every call on record",
  scenes: [
    {
      id: "send",
      title: "Everything is an agent; agents answer send",
      nodes: [
        { id: "you", label: "you", kind: "human", wide: { x: 300, y: 470 }, tall: { x: 280, y: 640 } },
        { id: "kernel", label: "kernel", sub: "routes every call", kind: "kernel", wide: { x: 640, y: 470 }, tall: { x: 280, y: 480 } },
        { id: "root", label: "kernel_state", sub: "the root", kind: "agent", w: 180, wide: { x: 640, y: 90 }, tall: { x: 280, y: 90 } },
        { id: "memory", label: "memory", sub: "memory store", kind: "store", w: 130, wide: { x: 460, y: 290 }, tall: { x: 75, y: 270 } },
        { id: "files", label: "files", sub: "file bridge", kind: "agent", w: 130, wide: { x: 600, y: 290 }, tall: { x: 210, y: 270 } },
        { id: "exec", label: "exec", sub: "a shell", kind: "agent", w: 130, wide: { x: 740, y: 290 }, tall: { x: 350, y: 270 } },
        { id: "ai", label: "ai", sub: "an AI", kind: "ai", w: 130, wide: { x: 880, y: 290 }, tall: { x: 485, y: 270 } },
      ],
      edges: [
        { id: "t1", from: "root", to: "memory", tree: true },
        { id: "t2", from: "root", to: "files", tree: true },
        { id: "t3", from: "root", to: "exec", tree: true },
        { id: "t4", from: "root", to: "ai", tree: true },
        { id: "e_you", from: "you", to: "kernel" },
        { id: "e_files", from: "kernel", to: "files" },
        { id: "e_mem", from: "kernel", to: "memory" },
      ],
      steps: [
        {
          caption: "A kernel holds a tree of agents under one root. Each agent is a record with a handler: a store, files, a shell, an AI.",
          show: ["root", "memory", "files", "exec", "ai", "t1", "t2", "t3", "t4"],
          focus: ["root", "memory", "files", "exec", "ai"],
        },
        {
          caption: "Every agent answers one verb, send: a target, and a payload that names the verb.",
          show: ["you", "e_you", "e_files"],
          flow: [{ edge: "e_you", label: "send files · read" }, { edge: "e_files", label: "read" }],
        },
        {
          caption: "Before anything runs, the kernel checks the grant on both sides: the caller's can_call and the target's callable_by. No grant, no call.",
          focus: ["kernel", "files"],
        },
        {
          caption: "The reply goes back to the caller, and the call and its outcome are written to the memory store under one pid.",
          show: ["e_mem"],
          flow: [
            { edge: "e_files", label: "result", back: true },
            { edge: "e_you", label: "result", back: true },
            { edge: "e_mem", label: "call + settle rows" },
          ],
        },
        {
          caption: "Nothing is guessed: reflect asks any agent what it is, which verbs it takes, and its readme.",
          flow: [
            { edge: "e_you", label: "reflect files" },
            { edge: "e_files", label: "reflect" },
            { edge: "e_files", label: "verbs · readme", back: true },
          ],
        },
      ],
    },
  ],
};
