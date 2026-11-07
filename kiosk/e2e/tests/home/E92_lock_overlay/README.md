# E92 `lock_overlay` — the kiosk lock

#55: the `lock` agent keeps the PIN; the Home page covers itself while its state says
`locked`. `system` (a folder named so: the Home's `systemRow`) and `alpha` run
`fantastic canvas`; `lock` is made in `system` as the recipe makes it, and the Home's
origin is added to the system canvas's `web`.

1. who may call it: the grant pass leaves `lock`'s `callable_by` with `user` and the
   canvas leg only, while `files` is open to `mcp`. `fantastic send` from the system
   folder (the `mcp` leg) to `lock` fails with `mcp may not call 'state' on lock`
   (`unauthorized`), to `files` it completes. A websocket with `Origin:
   http://example.invalid` gets 403 and `web` does not list it. Where the handler is
   registered (Linux), `user.forward` to `lock` `state` answers with `locked`, `has_pin`,
   `auto_lock_min`, `failures` and `retry_at_ms`; elsewhere it prints
   `SKIP lock_overlay/state`.
2. the overlay, on every OS, from `state` events pushed through the wire's `emit`:
   `locked: false` leaves it hidden and nothing inert; `locked: true` shows
   `.lock-overlay` and makes every other child of `#home` `inert`; Esc, mod+0/1/2, a
   click on the nav pill and on a row change neither the overlay nor the home agent's
   selection; a PIN tried shows a refusal in `.lock-note` and the overlay stays;
   `locked: false` takes the overlay and the `inert` away.
3. the real lock, only where `lock.state.available` is true (else
   `SKIP lock_overlay/overlay: lock agent unavailable: <why>`): `set_pin 4321`, `lock`;
   the overlay shows; `0000` is refused; `4321` hides it, removes `inert`, and `state`
   says `locked: false`.
