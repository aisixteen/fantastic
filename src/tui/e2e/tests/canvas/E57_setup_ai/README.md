# E57 `setup_ai` — the dedicated AI set-up test

Owner 2026-09-30: AI set-up is VERY important (its guided UX is designed later, together). Other tests PRESET an AI; this one drives the set-up itself, from an EMPTY home:

1. action popup → `agent` → the set-up cell;
2. pick the plug's backend; fill its schema fields from the plug's record (`id` = `helper`); WRITE THE RECORD;
3. SAVE AS TEMPLATE `mine`; make it the default;
4. a free-text spot → a new AI cell on the default template — it answers;
5. the sidebar's `helper` → a chat cell — it answers.

**PASS**: the `helper` record exists, `mine.yaml` is saved and preferred, and both cells answered in words. (The key comes from the environment — the plug's own variable — not the key store.)
