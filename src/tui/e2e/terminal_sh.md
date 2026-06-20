# Scenario: @sh breathing terminal

Run a shell command from the chat; its live PTY renders as a "breathing" viewport
that sizes to the output. Short output → a short viewport.

```script
wait 1500
key space
wait 500
type @sh echo hello-from-sh
key enter
wait 900
shot sh_output
```

## Expect
- A `→ sh sh` activity line in the transcript, and below the transcript a small
  PTY **viewport** with a `│ sh` header showing the shell output, including the
  line `hello-from-sh` (the command echo + its result).
- The viewport is only a few rows tall (it breathes to the content), not the
  whole screen.

## Pass / fail
PASS if `hello-from-sh` appears in a short bordered-by-rail viewport. FAIL if no
viewport renders or it eats the whole screen for a one-line command.

## Notes
The PTY runs in the harness's temp cwd, so the shell starts there.
