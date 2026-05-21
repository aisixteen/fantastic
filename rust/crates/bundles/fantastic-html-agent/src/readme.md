# html_agent — UI as a record
Body at `<agent_dir>/index.html`. Verbs: `render_html` (returns the stored body), `set_html` (writes a new body and emits `reload_html` on self so connected tabs refresh). Editable in any text editor — no JSON-escape gymnastics.
