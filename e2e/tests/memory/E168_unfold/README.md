# `unfold` (E168) — the model unfolds a folded reply with `recall` (map-once)

Real model, Rust lane, `costs: turns`. One AI chat and one `file_bridge` agent (`notes`)
holding `big.txt`: 100 lines of 800 characters, so the `read` reply (over 64 000
characters, the autofold size of 16 000 tokens) folds once it is answered. Line 80 is
`KEY=<token>`, minted at run time: the marker's head (the body's first line) does not
carry it and the task text never names it.

Three messages, each driven to the end of its chain:

1. "Read `big.txt` through `notes`. Answer only `ok`." — the reply arrives whole.
2. An unrelated small task — one more answered turn: the fold is in force.
3. "What is the token on the line that starts with `KEY=` in `big.txt`? Do not read the
   file again." — the body is only in the store: the model must send itself `recall` with
   the folded row's id (the existing verb; the marker in the request spells the call out).

Scored from the store: the rows, and each answer row's `attributes.parts` (the form,
bytes and hash of every part of the request it answered, replayed from its deltas):

- the read reply of message 1 is stored whole;
- the first request of message 3 shows the read row reduced (`anchor`, under 2 000
  bytes: its tag line and the type's marker) and no part of it holds the body;
- that row is the fold marker, not a strip stub: no view array lists it at `res: stub`;
- after message 3 the model's `recall` names the read row's id, the reply to that recall
  holds the token and the answer right after it does (the token came by the unfold);
- no call of any verb reaches `notes` after message 1 (no read, grep or list);
- no answer of the run opens with a time (`[2026-…`), `[thought` or a marker opener (the
  copied-head symptom map-once removes).

The free twin is E141 (`kernel/autofold`): the marker text in the request equals the
type's template.

Run (when e2e runs with a model are allowed): `--id unfold` on the memory group.
