# Scenario: per-character rooms + Shift-Tab + the @sh viewport stays put

Paste messages into different rooms, switch between them with Shift-Tab, and prove
each room shows ONLY its own chat — and that the `@sh` breathing terminal viewport
renders in the `@sh` room only (regression guard for the "htop streams to every
room" bug).

```script
wait 2500
key space
wait 600
# @ai room: a message (will error-seal without a model, but it's the ai room's)
type @ai hello from the ai room
key enter
wait 1500
shot ai_room
# address the kernel → opens + enters its room; only its exchange shows
type @kernel list_agents
key enter
wait 2500
shot kernel_room
# @sh room: start a long-lived TUI program; the viewport breathes HERE
type @sh top
key enter
wait 1500
shot sh_room
# Shift-Tab back toward @ai; the sh viewport must NOT bleed into other rooms
key shift-tab
wait 400
shot after_one_shifttab
key shift-tab
wait 400
shot after_two_shifttab
```

## Expect

- `ai_room`: tab bar `@ai @sh @ws` (active `@ai` bold); the `you: hello…` line and
  a sealed `✗ …` (no model) — the AI room's own content, prompt `@ai ▸`.
- `kernel_room`: a new `@kernel` chip appears and is active; body shows ONLY the
  `→ list_agents` + `kernel: {…core…}` reply (no `@ai` note/messages); `@kernel ▸`.
- `sh_room`: active `@sh`; the breathing PTY viewport (a `│ sh` header + `top`
  output) renders below the transcript; prompt `@sh ▸`.
- `after_one_shifttab` / `after_two_shifttab`: the active chip advances (`@sh →
  @ws → @kernel/@ai …`); in any room that is NOT `@sh`, the terminal viewport is
  GONE — no `top`/PTY rows bleed in. Each room shows only its own history.

## Pass / fail

PASS if (1) each room renders only its own messages, (2) addressing `@kernel`
opened and switched to its room, (3) the `@sh` `top` viewport shows in the `@sh`
room and DISAPPEARS after Shift-Tab to another room, and (4) the tab bar tracks
the active room. FAIL if the PTY viewport appears in a non-`@sh` room (the bug),
or rooms' histories bleed together.
