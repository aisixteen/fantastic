# `contract/home/` — the `home` protocol, as DATA

What the home's protocol promises a client, stated once and replayed by both
sides of it. The reference is the `home` agent's declared verbs (`src/tui/crates/fantastic-home-kernel/src/home/verbs.rs`, rendered by `docs of=home`).

```
contract/home/
  cases.json   protocol fantastic.home: its verbs, events, statuses and
               actions; a scripted machine (fixture); the cases, in order;
               valid and invalid states
```

## What it pins

- **The vocabulary**: `verbs`, `events`, `statuses` and `actions`, exactly.
- **The behaviour**: every case is one call to `home` on the fixture's machine,
  and what it must answer: a state with given fields, a refusal with its
  `reason` and `arg`, the payloads it emits, the folders it starts or stops, and
  what it sends to fantastics' kernels. The cases cover every verb and every
  refusal kind. `cases.json`'s `_doc` defines each key.
- **The state's shape**: `states.valid[0]` is the exact state the `first-look`
  case answers (golden); `states.invalid` are states a client must refuse, each
  with the field it must name.

## Who replays it

- **Rust, the agent**: `src/tui/crates/fantastic-home-kernel/src/home/contract_tests.rs`
  (`cargo test` in `src/`). It runs every case in order through `agent::call`
  against one runner on `FakeOs::from_fixture`, then holds the vocabulary to
  the agent (the declared verbs and their rendered docs, the `Status` serde
  names, every value `Row::actions` returns) and the golden to `first-look`.
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
