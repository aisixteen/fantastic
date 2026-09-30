# E47 `pdf_panel`

Inferred from dara (PRD, patent spec, mockups as PDF panels; the blank-PDF sandbox fix). A read-only `files` file leg under `web` allows only `docs/`; `secret/plan.pdf` sits beside it.

**PASS**: a cell's html frames or embeds `docs/prd.pdf` at a URL that really serves the PDF (the record is the evidence — a sandboxed panel's inner frame is invisible to the page). Reported: whether the panel is `trusted` (only then does the PDF paint — the sandbox blocks the viewer).
