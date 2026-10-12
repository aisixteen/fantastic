# E58 `prototype`

THE PROTOTYPING FLOW, yes/no (owner, 2026-10-04: the canvas is a garage, a
prototyping tool — not production grade, no packaging). An EMPTY canvas: no
project files, no exec agent seeded. One AI cell, two turns.

1. **Build:** a script that prints a counter (one line a second), an exec agent
   that may run exactly it, ONE panel with Start, Stop and a live log of the
   script's lines — wired by the AI itself, nothing built or bundled.
2. **Change:** count by 10, and show the last value big in its own panel.

Scored by clicking, not by the AI's words (`_lib/dev_surface` frame helpers):

- turn 1: a script file on disk · an exec agent whose `allowed` names it ·
  Start → the counter grows on the page · Stop → it stops growing;
- turn 2: Start → three consecutive multiples of 10 on the page · one panel
  showing one number, a multiple of 10 (the big value; a label beside it is fine) · Stop.

**PASS** = every check of both turns. A fail answers "no, for now"; the
summary names the step that broke.
