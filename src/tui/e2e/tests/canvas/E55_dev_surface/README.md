# E55 `dev_surface`

Inferred from dara's Compose Control (buttons → exec → live log), the terminals and the embedded webapp. A tiny project (`app/title.txt`, `build.sh` → `dist/index.html`) and an exec agent `dev` allowed exactly `sh build.sh` and `python3 -m http.server <port> --directory dist`. The AI is asked for controls, a build log, a run log, the live app and a status, wired to update by themselves.

**PASS**: ≥ 2 cells made (controls + log + status in one, the app in another — owner 2026-10-06: less filler, same complexity), and E56's reactive check passes on them (`_lib/dev_surface.reactive_check`): Build logs `built: Hello v1` · Run shows running and the app frame shows `Hello v1` · Stop shows stopped · after editing the title, Build + Run show `Hello v2` — no page reload.
