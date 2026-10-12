# E57 `setup_ai` — the dedicated AI set-up test

Owner 2026-09-30: AI set-up is VERY important. Other tests PRESET an AI; this one drives the set-up WIZARD itself (owner 2026-10-11), from an EMPTY home, live on the BACKEND plug:

1. action popup → `AI agent` → the wizard's page 1 → the plug's provider;
2. page 2: its URL, model and required fields from the plug's record; the key is the one an earlier set-up stored (picked as the saved key for that host);
3. create & open chat → a new AI cell on the new saved set-up — it answers;
4. its ⚙ settings show the same URL.

**PASS**: `<provider>-1.yaml` is saved and preferred, the cell answered in words, and its settings show its URL.
