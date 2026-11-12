# fantastic-home-kernel

The home kernel as a library: what a Rust shell links to host the home
in-process, the way the desktop app does. It boots a minimal kernel in
`<home>/app` on the caller's tokio runtime: its own sqlite store, a loopback
web host that serves the Home (`/canvas_page/file/home.html`) and the example
client (`/canvas_page/file/list.html`), and the `home` agent, which binds the
pure ViewModel (`fantastic-home`, `../fantastic-home/README.md`) to the kernel
protocol. No exec, no file leg, no AI.

The protocol every client speaks, this library's `call` and `events` included,
is the `home` agent's declared verbs (`docs of=home` renders them). `src/home/` is the
reference OS layer (`sys`, the one place that touches the OS, behind the
`HomeOs` trait) and effect runner (`runner.rs`) for a shell that links only
`fantastic-home`.

## Host it

- **Boot** with `HomeKernel::boot(Config {home, port, fantastic_bin})` from
  inside your tokio runtime. `home` is the fantastic home dir,
  `home_dir_from_env()?` in an app. `port: None` keeps the recorded port (a
  free one the first time).
- **`fantastic_bin` is explicit**: the absolute path of the `fantastic` binary
  the home runs as `fantastic canvas` (and, on Windows, as its stop helper).
  Never `current_exe()`, which is your shell. The fantastics it starts outlive
  the host: they keep running after `shutdown`, and the next boot adopts them.
- **Show it**: `url()` is the Home's URL, for your webview.
- **`call(payload)`** sends `payload` to `home` as `user` does (a page's calls
  take the same path), so the grants apply. A refusal is `Err("<error> — <hint>")`.
- **`events()`** is every payload sent or emitted on `home` from now on: the
  `state` emits, `focus` and `menu_spec` emits, and the calls other clients
  make, mirrored. Keep the types you read, and drop a `state` whose `seq` is not
  above the last one you drew.
- **`focus`**: when `{"type":"focus"}` arrives on `events()`, raise your
  window. Another client (a second launch handing over) sends it.
- **Stop in order** with `shutdown()`: the kernel's drain, the records, the
  store, the run file, then the lock.
- **One host per home.** `running(home)` names the host that serves now (the
  lock's holder, as its run file `<home>/app/home.yaml` names it). If there is
  one, `hand_over(to, paths)` sends it the folders to open (`open`) and asks it
  to show itself (`focus`); then your process leaves.
