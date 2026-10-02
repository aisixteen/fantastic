import type { Section } from "../model.ts";

export const aiContext: Section = {
  id: "ai",
  title: "How an AI agent's context works",
  blurb: "Assembled each turn, acted on with call tags",
  scenes: [
    {
      id: "turn",
      title: "One turn of an AI agent",
      nodes: [
        { id: "you", label: "you", kind: "human", wide: { x: 90, y: 280 }, tall: { x: 140, y: 46 } },
        { id: "ai", label: "AI agent", sub: "ai_core", kind: "ai", wide: { x: 300, y: 280 }, tall: { x: 140, y: 152 } },
        { id: "kernel", label: "kernel", sub: "the AI's own grants", kind: "kernel", wide: { x: 300, y: 470 }, tall: { x: 420, y: 152 } },
        { id: "ctx", label: "context", sub: "assembled each turn", kind: "ui", w: 190, wide: { x: 600, y: 280 }, tall: { x: 280, y: 426 } },
        { id: "sys", label: "call grammar", sub: "how to call", kind: "agent", wide: { x: 480, y: 90 }, tall: { x: 100, y: 289 } },
        { id: "primer", label: "what it reflected", sub: "readmes · verbs", kind: "agent", wide: { x: 720, y: 90 }, tall: { x: 460, y: 289 } },
        { id: "history", label: "conversation", sub: "memory store rows", kind: "store", wide: { x: 600, y: 470 }, tall: { x: 140, y: 578 } },
        { id: "model", label: "model", sub: "streams raw text", kind: "ext", wide: { x: 880, y: 280 }, tall: { x: 420, y: 578 } },
      ],
      edges: [
        { id: "e_you", from: "you", to: "ai" },
        { id: "e_asm", from: "ai", to: "ctx" },
        { id: "e_sys", from: "sys", to: "ctx" },
        { id: "e_pr", from: "primer", to: "ctx" },
        { id: "e_hist", from: "history", to: "ctx" },
        { id: "e_model", from: "ctx", to: "model" },
        { id: "e_kern", from: "ai", to: "kernel" },
      ],
      steps: [
        {
          caption: "You talk to an AI agent with send, like to any other agent.",
          show: ["you", "e_you"],
          flow: [{ edge: "e_you", label: "chat" }],
        },
        {
          caption: "Each turn, ai_core assembles the context: the call grammar, what the AI learned by reflecting, and the conversation so far.",
          show: ["ctx", "sys", "primer", "history", "e_asm", "e_sys", "e_pr", "e_hist"],
          flow: [
            { edge: "e_sys", label: "grammar" },
            { edge: "e_pr", label: "readmes" },
            { edge: "e_hist", label: "history" },
          ],
        },
        {
          caption: "The provider only streams raw text. To act, the model writes call tags such as <call name=\"send\">; there is no provider tool API.",
          show: ["model", "e_model"],
          flow: [
            { edge: "e_model", label: "prompt" },
            { edge: "e_model", label: "text + <call> tags", back: true },
          ],
        },
        {
          caption: "ai_core parses the tags and sends the calls through the kernel, under the AI's own grants.",
          show: ["kernel", "e_kern"],
          flow: [
            { edge: "e_asm", label: "calls", back: true },
            { edge: "e_kern", label: "send" },
          ],
        },
        {
          caption: "The results come back as the next turn, and every turn is stored as rows in the AI's memory store.",
          flow: [
            { edge: "e_kern", label: "results", back: true },
            { edge: "e_hist", label: "rows", back: true },
          ],
        },
        {
          caption: "When the window fills up, old turns are compacted, and a context notice tells the model it can recall them.",
          focus: ["ctx", "history"],
        },
      ],
    },
  ],
};
