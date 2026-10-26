# E130 import_twice

`user.import_subtree` (#269) against a running `fantastic canvas`, called the way
`fantastic send` does. A hand-written blueprint (`squad` > `lead`, `scout`; `lead`
reaches the port `@memory`) is imported under `team_home`:

1. the preview (no `approve`) names the ids, what each reaches and the ports, and creates nothing;
2. `approve: true` makes three agents (`*_1`), parents right on disk, grants re-wired, and the bound store `memory2` is `callable_by` the new `lead_1`;
3. a second import is a second copy (`*_2`); editing copy 1 leaves copy 2 as it was;
4. with the port unbound the call is refused (`unbound_ports`) and nothing is created.

Scripted (no model, no key); needs no browser.
