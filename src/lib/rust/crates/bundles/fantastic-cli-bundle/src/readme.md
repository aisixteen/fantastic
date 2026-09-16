# cli — stdout renderer
Renders token/done/say/error events to stdout. Ephemeral — composed per-process when stdin is a tty, never persisted.

A DUMB SINK for the PTY intro too: it prints what it is told and never inspects the tree. `intro_booting` (kernel→cli, pre-boot) prints the identity + pull/push control-plane map; agents announce their OWN endpoints during boot (e.g. web sends/publishes a `say` with its listening URL); `booted` (kernel→cli, post-boot) is the "all booted" close. Best-effort, tty-only.
