# fantastic-home

The fantastic manager's model and ViewModel, for Rust shells: everything the
home knows about the fantastics on this machine, as plain data, and the home
itself as events in, effects out.

```text
  model      the registry entries and a folder's run file, parsed from text
  probe      what one fantastic looks like, asked one question at a time
  viewmodel  the home: events in, effects out, rows to draw
```

**It is pure.** Nothing here reads a file, a socket, a process, the clock or
the environment. Text arrives as `&str`, time arrives on events as `now_ms`,
and every question for the machine leaves as data, for the shell's OS layer to
answer. `tests/purity.rs` scans the sources to keep it so.

Link this crate when your shell has an OS layer of its own. To host the home
with the reference OS layer, link `fantastic-home-kernel` instead
(`../fantastic-home-kernel/README.md`): its `src/home/` is the reference OS
layer (`sys`, behind the `HomeOs` trait) and effect runner (`runner.rs`), and
the `home` protocol (`docs/home.md` there) is this crate's state on the wire.

## The loop

```text
  OS layer ──Event──▶ Home::reduce ──Vec<Effect>──▶ OS layer
                       Home::rows(now_ms) ──▶ the list to draw
```

The shell stamps every event that needs the time with `now_ms`, runs each
effect, and answers with the matching event:

| effect | the shell runs | and answers |
|---|---|---|
| `ReadRegistry` | read every `<home>/fantastics/<folder>/fantastic.yaml` | `RegistryRead { now_ms, entries }` (folder, text), or `RegistryFailed { error }` |
| `Probe { jobs }` | each job through a `Prober` (below) | `Probed { now_ms, results }` (job key, probe) |
| `Spawn { path }` | start `fantastic canvas` in `path` | `Spawned { path, result }` |
| `Terminate { path }` | tell the process holding `path` to stop | `Terminated { path, sent }`, or `StopFailed { path, error }` |
| `Register { path, port }` | `register_home_folder {value: true}` to `user_store` through the kernel on `port` | `RegisterDone { path, result }` |
| `SwitchOff { port }` | `register_home_folder {value: false}` to `user_store` on `port` | `Forgot { id, result }` |
| `Unstore { port, guid }` | `unstore {guid}` to `user_store` on `port` | `Forgot { id, result }` |

The user's events need no effect first: `Tick { now_ms }` (look again; the
reference shell ticks every 2 s), `Start { id, now_ms }`, `Stop { id }`,
`Open { paths, now_ms }`, `Forget { id }`, `Select { id }`,
`SelectPage { index }`, `ClosePage { id }` and `Dismiss`. At boot,
`Restore { pages, selection }` gives back the pages and selection the shell
kept from its last run; the home holds it until the first `RegistryRead`, the
first moment a page's id can be checked.

## The five questions

A `Prober` holds one probe procedure: `next()` is the question it needs
answered (`None` once it is done), `answer()` feeds it, `probe()` is what is
known. Every OS layer must answer exactly as the reference one
(`fantastic-home-kernel/src/home/sys`) does:

| `Ask` | the exact answer |
|---|---|
| `IsFantastic { path }` | `Answer::IsFantastic`: `<path>/.fantastic` exists and is a directory |
| `LockHolder { path }` | `Answer::LockHolder`: the pid in the folder's lock file if `pid_alive(pid)`, and then only if the lock is held (`lock::held`); a dead pid's stale file is never asked for its lock |
| `RunFile { path }` | `Answer::RunFile`: `CanvasRun::parse` of `<path>/` + `RUN_FILE`; unreadable or invalid is `None` |
| `CanvasUp { canvas }` | `Answer::CanvasUp`: an HTTP GET of `canvas.url` answers a status in 200–399 within 1.5 s |
| `Guid { port }` | `Answer::Guid`: `reflect` (`tree: "none"`) on the root, sent as `user.forward` to `kernel` over `ws://127.0.0.1:<port>/user/ws`, answers a non-empty `guid` within 3 s |

An answer of the wrong kind for the pending question is a bug of the OS layer,
and panics naming both.

## Two modes, one own folder

`Config::new(Mode::Lock)` is the full app: it reads folders and locks, and a
fantastic is `running` when its folder's lock holder is the registered pid and
its canvas answers with its guid. `Mode::Canvas` is a sandboxed app: it may only
ask canvases, trusts the registry for the rest, and never starts or stops
anything. `Config::own_folder(path)` names the folder the home itself runs in:
a registry entry with that path is never listed.

Every user-facing text is `Home::problem()`: a shell shows it and sends
`Dismiss` once it is read. A shell writes no message of its own.

## A whole shell, with no I/O

One look over a scripted machine: a tick, the registry, the probe of each job,
then the rows. A real shell answers the same effects and questions from the OS.

```rust
use fantastic_home::{Answer, Ask, Canvas, CanvasRun, Config, Effect, Event, Home, Mode, Prober, Status};

let now_ms = 1_760_800_000_000;
let url = "http://127.0.0.1:8001/canvas_page/file/index.html";
let mut home = Home::new(Config::new(Mode::Lock));

// look: the home asks for the registry
assert_eq!(home.reduce(Event::Tick { now_ms }), vec![Effect::ReadRegistry]);

// the shell reads it: one entry, `<home>/fantastics/notes/fantastic.yaml`
let entries = vec![(
    "notes".to_string(),
    format!("guid: g-1\npath: /w/notes\npid: 41\ncanvas: {url}\nstored_at: 1\nlast_seen: 1760799000\n"),
)];
let jobs = match home.reduce(Event::RegistryRead { now_ms, entries }).as_slice() {
    [Effect::Probe { jobs }] => jobs.clone(),
    other => panic!("expected one Probe, got {other:?}"),
};

// the scripted machine: /w/notes runs, held by 41, its canvas answers as g-1
let machine = |ask: &Ask| match ask {
    Ask::IsFantastic { .. } => Answer::IsFantastic(true),
    Ask::LockHolder { .. } => Answer::LockHolder(Some(41)),
    Ask::RunFile { .. } => Answer::RunFile(CanvasRun::parse(&format!("pid: 41\nport: 8001\nurl: {url}\n"))),
    Ask::CanvasUp { .. } => Answer::CanvasUp(true),
    Ask::Guid { .. } => Answer::Guid(Some("g-1".to_string())),
};

// each job, one question at a time
let mut results = Vec::new();
for job in jobs {
    let mut prober = Prober::new(job.target, job.mode);
    while let Some(ask) = prober.next() {
        prober.answer(machine(&ask));
    }
    results.push((job.key, prober.probe().clone()));
}
assert!(home.reduce(Event::Probed { now_ms, results }).is_empty());

// draw
let rows = home.rows(now_ms);
assert!(home.listed());
assert_eq!(rows.len(), 1);
assert_eq!(rows[0].name, "notes");
assert_eq!(rows[0].status, Status::Running(Canvas { url: url.to_string(), port: 8001 }));
assert_eq!(rows[0].actions(Mode::Lock), ["stop", "open", "remove"]);

// the user stops it: the home asks the shell to terminate the holder
assert_eq!(
    home.reduce(Event::Stop { id: "g-1".to_string() }),
    vec![Effect::Terminate { path: "/w/notes".to_string() }]
);
assert_eq!(home.problem(), None);
```
