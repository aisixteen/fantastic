# Scenario: chat + kernel routing

Press a key to enter the chat, then drive the in-proc manager kernel with a sugar
command and check the reply renders.

```script
wait 2500
key space
wait 600
shot chat_empty
type @kernel list_agents
key enter
wait 2500
shot kernel_reply
```

## Expect
- `chat_empty`: a **borderless** chat floating over the starfield — a status line
  `host: N agents · ws: none · @ai · @sh · @ws · /intro · Ctrl+F focus`, a system
  help note with a `│` rail, and a visible input prompt `@ai ▸` at the bottom.
  Stars show in the ~2-cell edge margin and the 1-row gap above the input.
- `kernel_reply`: a `→ list_agents kernel` activity line, then a reply line
  `kernel: { "agents": [ { "id": "core" } … ] }`, and the input prompt now reads
  `@kernel ▸` (the sticky target retargeted).

## Pass / fail
PASS if the kernel reply with `core` renders in the transcript and the prompt is
readable. FAIL if input is invisible, the reply is missing, or borders box the chat.
