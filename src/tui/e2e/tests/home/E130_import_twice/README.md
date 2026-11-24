# E130 import_twice

`user.import_subtree` (#269) against a running `fantastic canvas`, called the way
`fantastic send` does. A hand-written blueprint (`squad` > `scout`, `lead`; `lead`
holds lines on `scout` and the port `@memory`) is imported under `team_home`:

1. the preview (no `approve`) names the ids, what each reaches and the ports, and creates nothing;
2. `approve: true` makes three agents (`*_1`), parents right on disk, the new lead holds lines on scout_1 and memory2; no target's record changes (scout_1 holds no line, memory2's reflect is byte-equal);
3. a second import is a second copy (`*_2`); editing copy 1 leaves copy 2 as it was;
4. with the port unbound the call is refused (`unbound_ports`) and nothing is created.

Scripted (no model, no key); needs no browser.
