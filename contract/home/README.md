# `contract/home/` — the `home` protocol, as DATA

What the home's protocol promises a client, stated once and replayed by both
sides of it. The reference is `src/tui/crates/fantastic-home-kernel/docs/home.md`.

```
contract/home/
  v1/cases.json   protocol fantastic.home v1: its verbs, events, statuses and
                  actions; a scripted machine (fixture); the cases, in order;
                  valid and invalid states
```

## What it pins

- **The vocabulary**: `verbs`, `events`, `statuses` and `actions`, exactly.
- **The behaviour**: every case is one call to `home` on the fixture's machine,
  and what it must answer: a state with given fields, a refusal with its
  `reason` and `arg`, the payloads it emits, the folders it starts or stops, and
  what it sends to fantastics' kernels. The cases cover every verb and every
  refusal kind (`missing_arg`, `bad_arg`, `not_found`, `unhandled`, a version
  other than 1). `cases.json`'s `_doc` defines each key.
- **The state's shape**: `states.valid[0]` is the exact state the `first-look`
  case answers (golden); `states.invalid` are states a client must refuse, each
  with the field it must name.

## Who replays it

- **Rust, the agent**: `src/tui/crates/fantastic-home-kernel/src/home/contract_tests.rs`
  (`cargo test` in `src/`). It runs every case in order through `agent::call`
  against one runner on `FakeOs::from_fixture`, then holds the vocabulary to
  the agent (`home::VERBS`, the `docs` verb texts, `emits_doc`, the `Status`
  serde names, every value `Row::actions` returns), the golden to `first-look`,
  and `docs/home.md` to every verb, status, field and event. It also asks the
  kernel's `docs` verb (`docs of=home tier=detailed`).
- **TS, the client**: `src/tui/web/canvas/tests/home_contract.test.ts`
  (`npm test` in `src/tui/web/canvas`). It holds the Home's model to the
  vocabulary (`STATUSES`, `ACTIONS`, a `label` per status), its grant
  (`HOME_VERBS`: the verbs a page sends, plus `watch`) and `decodeState` to the
  states, and checks the example client `list.html`.

## The rule

A case is DATA: it says what is true of the protocol, never how a runtime gets
there. Adding a case puts both replays on the hook, and a runtime that cannot
run one skips it BY NAME, with the reason in its driver — never silently. The
Rust replay runs every case today; the TS replay holds the client to the
vocabulary and the states.

Changing what a v1 case says is the compatibility rule's business
(`docs/home.md`, "Versioning"): v1 only grows. Anything that is not an addition
is `v2/`.
