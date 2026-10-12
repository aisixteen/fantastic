# E72 `setup_wizard` — the AI set-up wizard, free

Owner 2026-10-11: the AI set-up is a two-page wizard (a provider, then its details), every page has SKIP, an AI cell that is not set up says how to set it up, and ⚙ on an AI cell is its settings. No model is called: the URL is unreachable on purpose.

1. `AI agent` → page 1 (four providers, vllm linked) → skip → an AI cell "not set up yet";
2. its ⚙ settings → the wizard for it → vllm → page 2 (name `vllm-1`, URL filled) → create: the saved set-up is saved as the default and the SAME cell is bound;
3. a turn fails (nothing listens) → the chat offers settings / a coding agent;
4. ⚙ on its head → its settings (its URL and model) → a changed model is saved on the live AI's record.
