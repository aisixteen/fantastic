# fantastic-home-kernel

The home kernel as a library: what a Rust shell links to host the home
in-process, the way the desktop app does. It boots a minimal kernel in
`<home>/app` on the caller's tokio runtime: its own sqlite store, a loopback
web host that serves the Home (`/canvas_page/file/home.html`) and the example
client (`/canvas_page/file/list.html`), and the `home` agent, which binds the
pure ViewModel (`fantastic-home`, `../fantastic-home/README.md`) to the kernel
protocol. No exec, no file leg, no AI.

The protocol every client speaks, this library's `call` and `events` included,
is `docs/home.md` (`docs of=home` serves it as data). `src/home/` is the
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

## Example

```rust,no_run
use std::path::PathBuf;

use fantastic_home_kernel::{hand_over, home_dir_from_env, running, Config, HomeKernel};
use serde_json::json;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), String> {
    let home = home_dir_from_env()?;
    let folders = vec!["/Users/me/notes".to_string()];

    // one host per home: hand the folders to the one that serves, and leave
    if let Some(host) = running(&home).await? {
        return hand_over(&host, folders).await;
    }

    let hk = HomeKernel::boot(Config {
        home,
        port: None,
        fantastic_bin: PathBuf::from("/Applications/Fantastic.app/Contents/MacOS/fantastic"),
    })
    .await?;
    println!("show {} in the window", hk.url());

    // watch first, then list: nothing is missed
    let mut events = hk.events();
    let state = hk.call(json!({"type": "list", "version": 1})).await?;
    let mut drawn = state["seq"].as_u64().unwrap_or(0);
    hk.call(json!({"type": "open", "paths": folders, "version": 1})).await?;

    tokio::spawn(async move {
        while let Some(payload) = events.recv().await {
            match payload["type"].as_str() {
                Some("state") => {
                    let seq = payload["seq"].as_u64().unwrap_or(0);
                    if seq > drawn {
                        drawn = seq;
                        // draw it, if you draw your own UI
                    }
                }
                Some("focus") => { /* raise the window */ }
                _ => {} // mirrored calls, and what this shell does not read
            }
        }
    });

    // … run your shell until the user quits, then:
    hk.shutdown().await
}
```
