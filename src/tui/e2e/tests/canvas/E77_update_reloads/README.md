# E77 `update_reloads` — a cell's new body shows at once

Owner 2026-10-13: a coding agent sent the view's `update {id, bytes, fields: {can_call}}`
(`fantastic send` to `<page guid>|view_root`) and the panel kept its old body until
the page was reloaded.

1. two html cells — trusted `t1`, sandboxed `s1` — each with body `v1`, `can_call {$self: *}`;
2. `update` each with body `v2` and a new `can_call` (the coding agent's call, verbatim shape);
3. each iframe shows `v2` within 5 s, with NO page reload; the record holds `v2`;
4. after a reload, still `v2`.
