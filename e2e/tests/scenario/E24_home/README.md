# E24 · the home — the in-process home kernel managing real fantastics

**Costs:** free. No model runs, and no browser.

The home kernel (`src/tui/crates/fantastic-home-kernel`) is a library whose only
real host is the desktop app. Here it runs in its test-only host,
`fantastic-home-host --fantastic <bin> --port N`, on a throwaway home dir, and
manages real `fantastic canvas` processes. Every call goes through the home's
own page socket (`user.forward` to `home` over `/user/ws`), the state stream is
read from `/home/ws`, and every step is checked on disk too.

| run | proves |
|---|---|
| `boot` | the host announces its url and a GET of it answers 200; `list` is `listed` with no rows; `<home>/.fantastic/app/home.yaml` names the host's pid, port and url; `<home>/.fantastic/app/.fantastic/home.db` exists |
| `second-host-refused` | a second host on the same home exits 2, naming the first host's pid |
| `open-adds` | `open` on a fresh folder A: within 30 s one row has A's canonical path, `running`, A's lock holder as its pid and a canvas; `<home>/fantastics/<guid>/fantastic.yaml` names that pid and canvas; the launch log exists |
| `stop` | `stop`: the stream shows `stopping`, then `stopped` within 20 s; the entry's `pid` and `canvas` are `null`; A's lock is free and its run file gone (on Windows: a hidden console, stopped by `fantastic __ctrl_break`) |
| `start` | `start`: `running` again within 30 s |
| `outside` | the runner starts `fantastic canvas` in B itself and turns B's switch on through B's `user_store`: B is listed `running`; a `stop` from the home stops it (on Windows: a canvas in another console) and B's entry says stopped |
| `remove` | B started again, then `remove`: B's registry folder is gone and B keeps running; the runner stops B |
| `own-folder` | `open` of `<home>/.fantastic/app` is refused `bad_arg`, and no registry entry names it |
| `restart-keeps-pages` | `select` A, then stop the host: it exits 0, its run file is gone, its lock free, A still runs; started again, once listed, `pages == [A]`, `selection == A`, A `running` (adopted) |
| `host-leaves-them-running` | stop the host: it exits 0, A's lock is still held and its canvas answers |

Teardown stops every fantastic the run started (`oslayer.stop_fantastic` on each
folder's lock holder), and kills any still alive after 20 s.
