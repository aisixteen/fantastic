# E155 · store_view — the canvas store view over a real `append`

Scripted, no model, with a browser (`costs: free`). #505 (the view reads the `dump` shape the stores send: `tables.rows`) and #508 (an `append` emits `appended`, so the view refreshes by itself).

A `ram_store.tools` agent is placed (its default view is the store view). Then:

| check | what |
|---|---|
| 0 | the store and its cell exist; the view reads the empty store: `no rows yet` |
| 1 | a real `append` of two rows by the caller (the forwarded call of `user`) answers `appended: 2` |
| 2 | `dump` answers the rows under `tables.rows`, each with its `owner` |
| 3 | the view, with no click, shows `OWNERS · 1 · 2 rows` and the owner's line `<owner>  2 rows  0 folds` (the `appended` event reached it) |
| 4 | a second `append` of one row: the view shows 3 rows by itself |
| 5 | no page error |
