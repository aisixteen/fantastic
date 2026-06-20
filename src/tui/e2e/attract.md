# Scenario: attract screen

The boot/attract screen — the arcade title card. Streams the first ~2.4s so the
title's top→bottom reveal is visible, then a settled frame.

```script
# let the title power-on reveal play, captured as a stream
stream 300 8
wait 200
shot settled
```

## Expect
- A **starfield** (scattered `.` `·` `*`) fills the screen.
- The big block-font word **FANTASTIC** is rendered with `█` cells and is
  **clearly legible** (you can read F-A-N-T-A-S-T-I-C — not a mushy blob),
  centred, and as large as fits the width.
- Across the stream frames, the title **appears top→bottom** (early frames show
  only the upper rows; later frames the whole word).
- A blinking line **`PRESS ANY KEY TO CONTINUE`** appears a couple rows below the
  title (present in some frames, absent in others — it blinks).
- No chat, no input box, no borders.

## Pass / fail
PASS if the settled frame shows a legible `FANTASTIC` over a starfield with the
prompt. FAIL if the word is unreadable/clipped, or the title/stars are missing.
