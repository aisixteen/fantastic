# E91 `system_views` — the system agents' cells

#55: power, network, audio, displays, notifications and lock are host agents, each
with its own html view (`fantastic-system-agents/src/views/<id>.html`) drawn as a cell
by `place {backend_id}`. The test runs on every OS: the agents are made by hand with
their real handler modules (`<id>.tools`), as `kern/system.rs` `ensure` makes them
(under `user`, `callable_by {user: verbs}`, `view {kind: html, bytes, calls: verbs}`,
module and verbs read from the agent's Rust source), but the OS ports are Linux's, so
each view is fed a fixture state through the wire's `emit` (what the agent does itself
after a change). The real OS is never touched.

1. each cell mounts (a CDP target, as it may be sandboxed);
2. the fixture is drawn: battery 71% and "on battery"; two audio rows at 70% and 25%;
   the SSIDs, the device and the saved connection; two notifications; an output with
   its mode; the lock's auto-lock minutes in the box;
3. `{available: false, why}` shows the why and no enabled control (power's carries
   its `can`, as the agent's own does);
4. each control driven (power suspend, audio range and mute, network deactivate and scan,
   notifications dismiss, displays brightness, lock Lock now and set auto-lock) makes a
   call on THAT agent (`[n] <id> call <verb>` in the host log), a verb the view's
   `calls` lists, and the err line shows the refusal; no other system agent gains a call.
   Only while the real agent does not report `available: true`: off Linux the handler is not
   registered, on Linux without the service it says `unavailable`. With a real OS behind
   it nothing is clicked (no run suspends a machine);
5. the view's source names no target but its own `ID` in `fantastic.send/watch/emit`,
   and the controls made no call on `kernel` or `exec`;
6. no page error.
