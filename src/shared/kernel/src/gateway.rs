//! `gateway` — the workspace-kernel gateway. The product is a kernel *manager*:
//! it never reimplements kernel verbs. Instead it attaches to (or spawns) a
//! sovereign `fantastic_kernel` daemon rooted in a working directory and drives
//! it over loopback HTTP, through the kernel's own web_rest serve surface.
//!
//! Shape (all verified against the real substrate artifacts, not assumed):
//! - A workspace dir gets `<dir>/.fantastic/`: `lock.json` = `{"pid":N}` (only
//!   while a daemon owns the dir), and `agents/<id>/agent.json` per agent —
//!   nested by parentage (children live under `<parent>/agents/<child>/`).
//! - The `web.tools` agent persists its bound `port`; the `web_rest.tools`
//!   agent (a child of web) serves `GET /<rest>/_reflect[/<id>]` and
//!   `POST /<rest>/<target>` with body `{"type":"<verb>",...}`.
//! - The root is `core`; the dispatch alias `kernel` also reaches it.
//!
//! The gateway is a thin shell: it only spawns / discovers / forwards. No
//! agentic logic, no embedded kernel.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

/// Which substrate kernel to drive. Only `Rust` is wired end-to-end today; the
/// others are structured in so the resolver/spawn paths can grow without a
/// reshape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Runtime {
    #[default]
    Rust,
    Python,
    Swift,
}

impl Runtime {
    /// The binary name each runtime exposes on PATH.
    fn bin_name(self) -> &'static str {
        match self {
            // All three substrates ship the same daemon entrypoint name.
            Runtime::Rust | Runtime::Python | Runtime::Swift => "fantastic_kernel",
        }
    }
}

/// Resolve the `fantastic_kernel` binary for a runtime:
///   1. `FANTASTIC_KERNEL_BIN` env (explicit override; honored verbatim).
///   2. `fantastic_kernel` on `$PATH` (manual scan — no extra crate).
///   3. Dev fallback: walk up from cwd for a `src/lib/rust` dir, then look for
///      `target/release/fantastic_kernel` then `target/debug/fantastic_kernel`.
///
/// Returns `None` if nothing is found.
pub fn resolve_kernel_bin(runtime: Runtime) -> Option<PathBuf> {
    let env_bin = std::env::var("FANTASTIC_KERNEL_BIN").ok();
    let name = runtime.bin_name();

    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(name)).collect())
        .unwrap_or_default();

    // Dev fallback only makes sense for the rust substrate (it's the one built
    // out of this repo's `src/lib/rust`).
    let mut dev_candidates: Vec<PathBuf> = Vec::new();
    if runtime == Runtime::Rust {
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(libdir) = find_ancestor_dir(&cwd, "src/lib/rust") {
                for profile in ["release", "debug"] {
                    dev_candidates.push(libdir.join("target").join(profile).join(name));
                }
            }
        }
    }

    resolve_kernel_bin_from(env_bin.as_deref(), &path_dirs, &dev_candidates)
}

/// Pure resolver core (no env / no cwd reads): the `FANTASTIC_KERNEL_BIN`
/// override wins if it points at an existing file; else the first existing
/// `$PATH` candidate; else the first existing dev candidate; else `None`. Each
/// list element is a fully-formed candidate *path* to the binary.
fn resolve_kernel_bin_from(
    env_bin: Option<&str>,
    path_dirs: &[PathBuf],
    dev_candidates: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(p) = env_bin {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    for cand in path_dirs {
        if cand.is_file() {
            return Some(cand.clone());
        }
    }
    for cand in dev_candidates {
        if cand.is_file() {
            return Some(cand.clone());
        }
    }
    None
}

/// The exact `_reflect` URL the gateway hits: `{base}/{rest}/_reflect[/{id}]`.
fn reflect_url(base: &str, rest: &str, id: Option<&str>) -> String {
    match id {
        Some(target) => format!("{base}/{rest}/_reflect/{target}"),
        None => format!("{base}/{rest}/_reflect"),
    }
}

/// The exact `send` URL the gateway POSTs to: `{base}/{rest}/{target}`.
fn send_url(base: &str, rest: &str, target: &str) -> String {
    format!("{base}/{rest}/{target}")
}

/// Walk up from `start` looking for a directory containing the relative
/// `rel` sub-path (e.g. `src/lib/rust`). Returns the joined path if found.
fn find_ancestor_dir(start: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let cand = dir.join(rel);
        if cand.is_dir() {
            return Some(cand);
        }
        cur = dir.parent();
    }
    None
}

/// A live connection to a workspace kernel's web_rest serve surface.
#[derive(Debug, Clone)]
pub struct KernelHandle {
    /// `http://127.0.0.1:<port>`.
    pub base_url: String,
    /// The `web_rest` agent id that fronts the REST surface.
    pub rest_id: String,
    /// The daemon pid from `lock.json`, if known (best-effort, never used for
    /// liveness — HTTP is the source of truth).
    pub pid: Option<u32>,
    client: reqwest::Client,
}

impl KernelHandle {
    fn new(base_url: String, rest_id: String, pid: Option<u32>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            base_url,
            rest_id,
            pid,
            client,
        }
    }

    /// `GET {base}/{rest}/_reflect[/{id}]`.
    pub async fn reflect(&self, id: Option<&str>) -> Result<Value> {
        let url = reflect_url(&self.base_url, &self.rest_id, id);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let v: Value = resp.json().await.with_context(|| format!("decode {url}"))?;
        Ok(v)
    }

    /// `POST {base}/{rest}/{target}` with a JSON `{"type":"<verb>",...}` body.
    pub async fn send(&self, target: &str, payload: Value) -> Result<Value> {
        let url = send_url(&self.base_url, &self.rest_id, target);
        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let v: Value = resp.json().await.with_context(|| format!("decode {url}"))?;
        Ok(v)
    }
}

/// What `Workspace::discover` extracts from the on-disk `.fantastic/` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub port: u16,
    pub rest_id: String,
    pub pid: Option<u32>,
}

/// A working directory the gateway can attach to or spawn a kernel in.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub dir: PathBuf,
}

impl Workspace {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn fantastic_dir(&self) -> PathBuf {
        self.dir.join(".fantastic")
    }

    /// Pure-filesystem discovery — NO network. Reads `lock.json` (pid,
    /// best-effort) and scans the (nested) `agents/**/agent.json` tree for the
    /// `web.tools` agent (→ its `port`) and the `web_rest.tools` agent (→ its
    /// id). Returns `None` if either is missing (e.g. a stale/half-seeded dir).
    pub fn discover(&self) -> Option<Discovered> {
        discover_in(&self.fantastic_dir())
    }

    /// Build a handle from discovery, then confirm liveness with an HTTP
    /// reflect-ping (status < 500). Returns `Some(handle)` if it answers, else
    /// `None` (treat as stale / not-running — pid is NOT used for liveness).
    pub async fn attach(&self) -> Result<Option<KernelHandle>> {
        let Some(d) = self.discover() else {
            return Ok(None);
        };
        let handle = KernelHandle::new(format!("http://127.0.0.1:{}", d.port), d.rest_id, d.pid);
        if reflect_ping(&handle).await {
            Ok(Some(handle))
        } else {
            Ok(None)
        }
    }

    /// Resolve the bin, run the seed one-shots, spawn the daemon detached, and
    /// poll `attach()` until it answers (~10s) — then return the live handle.
    pub async fn spawn(&self, runtime: Runtime) -> Result<KernelHandle> {
        // A discoverable-but-dead workspace is a stale lock. Don't auto-kill;
        // surface a clear message so the user clears it deliberately.
        if let Some(d) = self.discover() {
            let handle =
                KernelHandle::new(format!("http://127.0.0.1:{}", d.port), d.rest_id, d.pid);
            if reflect_ping(&handle).await {
                return Ok(handle);
            }
            return Err(anyhow!(
                "stale workspace at {}: .fantastic discovered (port {}{}) but no kernel answered. \
                 Clear it with `rm -rf {}/.fantastic` (or stop the dead daemon) and retry.",
                self.dir.display(),
                d.port,
                d.pid.map(|p| format!(", lock pid {p}")).unwrap_or_default(),
                self.dir.display(),
            ));
        }

        let bin = resolve_kernel_bin(runtime).ok_or_else(|| {
            anyhow!(
                "no fantastic_kernel binary found (set FANTASTIC_KERNEL_BIN, put it on PATH, \
                 or build src/lib/rust with `cargo build --release -p fantastic-cli`)"
            )
        })?;

        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create workspace dir {}", self.dir.display()))?;

        let port = free_loopback_port()?;
        self.seed(&bin, port)?;

        // Spawn the daemon detached, logs → .fantastic/serve.log.
        let fdir = self.fantastic_dir();
        std::fs::create_dir_all(&fdir).with_context(|| format!("create {}", fdir.display()))?;
        let log =
            std::fs::File::create(fdir.join("serve.log")).with_context(|| "create serve.log")?;
        let log_err = log.try_clone().with_context(|| "clone serve.log handle")?;
        std::process::Command::new(&bin)
            .current_dir(&self.dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log))
            .stderr(std::process::Stdio::from(log_err))
            .spawn()
            .with_context(|| format!("spawn daemon {}", bin.display()))?;

        // Poll attach() until the web surface answers, ~10s budget.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(handle) = self.attach().await? {
                return Ok(handle);
            }
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "kernel spawned but its web surface never answered on port {port} \
                     (see {}/serve.log)",
                    fdir.display()
                ));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Attach to an already-running kernel; spawn one if none is live.
    pub async fn attach_or_spawn(&self, runtime: Runtime) -> Result<KernelHandle> {
        if let Some(handle) = self.attach().await? {
            return Ok(handle);
        }
        self.spawn(runtime).await
    }

    /// Run the proven serve-surface seed chain as one-shots (mirrors
    /// `boot_bare_host.sh`): store → web(port) → web_ws → web_rest. Each is a
    /// blocking `fantastic_kernel` invocation that mutates `.fantastic/`.
    fn seed(&self, bin: &Path, port: u16) -> Result<()> {
        // store: the file_bridge persistence surface (rooted at .fantastic).
        self.one_shot(
            bin,
            &[
                "core",
                "create_agent",
                "handler_module=file_bridge.tools",
                "id=store",
                "root=.fantastic",
                "ingress_rule=allow_all",
            ],
        )?;
        // web: binds the loopback port.
        let web = self.one_shot(
            bin,
            &[
                "core",
                "create_agent",
                "handler_module=web.tools",
                &format!("port={port}"),
            ],
        )?;
        let web_id =
            json_id(&web).ok_or_else(|| anyhow!("seed: web one-shot returned no id: {web}"))?;
        // web_ws onto web (opens the WS edge).
        self.one_shot(
            bin,
            &[
                &web_id,
                "create_agent",
                "handler_module=web_ws.tools",
                "ingress_rule=allow_all",
            ],
        )?;
        // web_rest onto web (the REST door the gateway drives).
        self.one_shot(
            bin,
            &[
                &web_id,
                "create_agent",
                "handler_module=web_rest.tools",
                "ingress_rule=allow_all",
            ],
        )?;
        Ok(())
    }

    /// One blocking `fantastic_kernel <args...>` in the workspace dir; returns
    /// the parsed reply JSON from stdout.
    fn one_shot(&self, bin: &Path, args: &[&str]) -> Result<Value> {
        let out = std::process::Command::new(bin)
            .current_dir(&self.dir)
            .args(args)
            .output()
            .with_context(|| format!("run {} {}", bin.display(), args.join(" ")))?;
        if !out.status.success() {
            return Err(anyhow!(
                "one-shot `{}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let v: Value = serde_json::from_str(stdout.trim())
            .with_context(|| format!("parse one-shot reply: {stdout}"))?;
        if let Some(err) = v.get("error").and_then(Value::as_str) {
            return Err(anyhow!("one-shot `{}` errored: {err}", args.join(" ")));
        }
        Ok(v)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Container backend.
//
// A workspace kernel can also run as a podman/docker container, driven over the
// SAME loopback-HTTP attach path as a native process: the workdir is
// bind-mounted, so `<dir>/.fantastic/` lands on the host and the existing
// `discover()` + `attach()` find the port + rest id and reflect-ping
// `127.0.0.1:<port>`. The container is just a different *launcher*; everything
// downstream is identical.
// ──────────────────────────────────────────────────────────────────────────

/// The minimal serve-surface seed steps, as pure data (argv vectors), in order:
/// store (file_bridge persistence rooted at `.fantastic`) → web (binds `port`)
/// → web_ws (the WS bus) → web_rest (REST door, id `rest`). The container path
/// replays these through `--entrypoint sh -c`; mirrors the native `seed` chain.
pub fn surface_steps(root: &str, port: u16) -> Vec<Vec<String>> {
    vec![
        vec![
            root.to_string(),
            "create_agent".into(),
            "handler_module=file_bridge.tools".into(),
            "id=store".into(),
            "root=.fantastic".into(),
            "ingress_rule=allow_all".into(),
        ],
        vec![
            root.to_string(),
            "create_agent".into(),
            "handler_module=web.tools".into(),
            "id=web".into(),
            format!("port={port}"),
        ],
        vec![
            "web".into(),
            "create_agent".into(),
            "handler_module=web_ws.tools".into(),
            "id=web_ws".into(),
            "ingress_rule=allow_all".into(),
        ],
        vec![
            "web".into(),
            "create_agent".into(),
            "handler_module=web_rest.tools".into(),
            "id=rest".into(),
            "ingress_rule=allow_all".into(),
        ],
    ]
}

/// In-container `(kernel bin path, root agent id)` per runtime. The root differs:
/// python roots at `kernel_state`; rust/swift at `core`.
fn container_spec(runtime: Runtime) -> (&'static str, &'static str) {
    match runtime {
        Runtime::Rust => ("/opt/fantastic/bin/fantastic-rust", "core"),
        Runtime::Python => ("/opt/fantastic/venv/bin/fantastic_kernel", "kernel_state"),
        Runtime::Swift => ("/opt/fantastic/bin/fantastic-swift", "core"),
    }
}

/// The `FANTASTIC_RUNTIME` env value each runtime carries into the container.
fn runtime_str(runtime: Runtime) -> &'static str {
    match runtime {
        Runtime::Rust => "rust",
        Runtime::Python => "python",
        Runtime::Swift => "swift",
    }
}

/// Resolve a container engine: `FANTASTIC_CONTAINER_ENGINE` (a path or name,
/// honored verbatim) → else a manual `$PATH` scan for `podman` (preferred) then
/// `docker` → else `None`. The bool is `is_podman` (drives `--userns=keep-id`).
fn container_engine() -> Option<(String, bool)> {
    if let Ok(e) = std::env::var("FANTASTIC_CONTAINER_ENGINE") {
        if !e.is_empty() {
            let is_podman = !e.to_lowercase().contains("docker");
            return Some((e, is_podman));
        }
    }
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for (name, is_podman) in [("podman", true), ("docker", false)] {
        for dir in &path_dirs {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some((cand.to_string_lossy().into_owned(), is_podman));
            }
        }
    }
    None
}

/// The container image to run — `FANTASTIC_IMAGE`, REQUIRED. By default, no
/// defaults: we never guess a tag that could run (or pull) an image you didn't
/// name. Unset → a clear error.
fn image() -> Result<String> {
    std::env::var("FANTASTIC_IMAGE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("set FANTASTIC_IMAGE — the container image is never guessed"))
}

/// Whether the image is present on the engine's host. NEVER pulls — a missing
/// image is a hard error the caller must resolve by building it. `image inspect`
/// exits 0 iff present (works for both podman and docker).
fn image_present(engine: &str, image: &str) -> bool {
    std::process::Command::new(engine)
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Pure builder for the `run -d` daemon argv. `--userns=keep-id` only when
/// podman (docker rejects it). Unit-tested so the exact vector is pinned.
fn run_daemon_args(
    name: &str,
    port: u16,
    abs_workdir: &str,
    runtime_str: &str,
    image: &str,
    is_podman: bool,
) -> Vec<String> {
    let mut a = vec!["run".to_string(), "-d".into(), "--name".into(), name.into()];
    if is_podman {
        a.push("--userns=keep-id".into());
    }
    a.extend([
        "-p".into(),
        format!("127.0.0.1:{port}:{port}"),
        "-v".into(),
        format!("{abs_workdir}:/work"),
        "-e".into(),
        format!("FANTASTIC_RUNTIME={runtime_str}"),
        "-e".into(),
        format!("FANTASTIC_PORT={port}"),
        image.into(),
    ]);
    a
}

/// Pure builder for the one-shot compose argv (`--entrypoint sh -c <script>`),
/// which replays the surface steps into the bind-mounted /work. `--userns` only
/// for podman.
fn compose_args(abs_workdir: &str, image: &str, is_podman: bool, script: &str) -> Vec<String> {
    let mut a = vec!["run".to_string(), "--rm".into()];
    if is_podman {
        a.push("--userns=keep-id".into());
    }
    a.extend([
        "-v".into(),
        format!("{abs_workdir}:/work"),
        "-w".into(),
        "/work".into(),
        "--entrypoint".into(),
        "sh".into(),
        image.into(),
        "-c".into(),
        script.into(),
    ]);
    a
}

/// Sanitize a dir basename into a container-name-safe token (`[a-z0-9_-]`).
fn sanitize_name(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "ws".into()
    } else {
        trimmed
    }
}

impl Workspace {
    /// Spawn the workspace kernel as a podman/docker container, then attach to it
    /// over the SAME loopback-HTTP path as a native process (the workdir is
    /// bind-mounted, so `.fantastic/` is on the host). Composes the serve surface
    /// once (skipped if already present), `run -d` the daemon, polls `attach()`
    /// for ~15s, writes `<dir>/.fantastic/launch.json`, and returns
    /// `(handle, container_name)`. On timeout, stops the container and errors.
    pub async fn spawn_container(&self, runtime: Runtime) -> Result<(KernelHandle, String)> {
        let (engine, is_podman) = container_engine().ok_or_else(|| {
            anyhow!(
                "no container engine found (set FANTASTIC_CONTAINER_ENGINE, or put podman/docker \
                 on PATH)"
            )
        })?;
        let img = image()?;
        if !image_present(&engine, &img) {
            return Err(anyhow!(
                "image {img} not present; build it (sh container/build.sh) — never pulled"
            ));
        }

        let (bin, root) = container_spec(runtime);

        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create workspace dir {}", self.dir.display()))?;
        let abs_dir = std::fs::canonicalize(&self.dir)
            .with_context(|| format!("canonicalize workspace dir {}", self.dir.display()))?;
        let abs_workdir = abs_dir.to_string_lossy().into_owned();

        let port = free_loopback_port()?;
        let basename = abs_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = format!("ft-ws-{}-{}", sanitize_name(&basename), port);

        // Compose the surface only if the web agent isn't already on disk (the
        // bind-mounted .fantastic survives across runs). `discover()` returning
        // None ⇒ not seeded yet.
        if self.discover().is_none() {
            let steps = surface_steps(root, port);
            let script = steps
                .iter()
                .map(|step| format!("{bin} {}", step.join(" ")))
                .collect::<Vec<_>>()
                .join(" && ");
            let args = compose_args(&abs_workdir, &img, is_podman, &script);
            let out = run_engine_blocking(&engine, &args).await?;
            if !out.status.success() {
                return Err(anyhow!(
                    "container compose failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
        }

        // Launch the daemon detached, port-mapped to loopback.
        let run_args = run_daemon_args(
            &name,
            port,
            &abs_workdir,
            runtime_str(runtime),
            &img,
            is_podman,
        );
        let out = run_engine_blocking(&engine, &run_args).await?;
        if !out.status.success() {
            return Err(anyhow!(
                "container run failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }

        // Poll the existing attach() — the bind-mounted .fantastic carries the
        // port + rest id, and reflect-ping confirms the in-container web surface.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(handle) = self.attach().await? {
                let launch = serde_json::json!({
                    "container": name,
                    "engine": engine,
                    "port": port,
                });
                let fdir = self.fantastic_dir();
                let _ = std::fs::create_dir_all(&fdir);
                let _ = std::fs::write(
                    fdir.join("launch.json"),
                    serde_json::to_string_pretty(&launch).unwrap_or_default(),
                );
                return Ok((handle, name));
            }
            if std::time::Instant::now() >= deadline {
                stop_container(&engine, &name);
                return Err(anyhow!(
                    "container {name} started but its web surface never answered on port {port}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Run `<engine> <args...>` to completion on a blocking thread (best for the
/// short-lived `run`/`compose` invocations), capturing output.
async fn run_engine_blocking(engine: &str, args: &[String]) -> Result<std::process::Output> {
    let engine = engine.to_string();
    let args = args.to_vec();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(&engine)
            .args(&args)
            .output()
            .with_context(|| format!("run {engine} {}", args.join(" ")))
    })
    .await
    .context("join engine command")?
}

/// Best-effort stop + remove a container by name (`stop -t 8` then `rm -f`).
/// Blocking; ignores errors (the container may already be gone).
pub fn stop_container(engine: &str, name: &str) {
    let _ = std::process::Command::new(engine)
        .args(["stop", "-t", "8", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::process::Command::new(engine)
        .args(["rm", "-f", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// HTTP reflect-ping: liveness = the web surface answers with status < 500.
async fn reflect_ping(handle: &KernelHandle) -> bool {
    let url = reflect_url(&handle.base_url, &handle.rest_id, None);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_default();
    match client.get(&url).send().await {
        Ok(resp) => resp.status().as_u16() < 500,
        Err(_) => false,
    }
}

fn json_id(v: &Value) -> Option<String> {
    v.get("id").and_then(Value::as_str).map(str::to_string)
}

/// Pick a free loopback TCP port by binding :0 and reading it back.
fn free_loopback_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").context("bind :0 for free port")?;
    Ok(l.local_addr().context("local_addr")?.port())
}

/// Filesystem discovery factored out of `Workspace` so it's unit-testable
/// against a fixture `.fantastic/` dir. `fdir` is the `.fantastic` directory.
fn discover_in(fdir: &Path) -> Option<Discovered> {
    let agents_root = fdir.join("agents");
    if !agents_root.is_dir() {
        return None;
    }

    let mut port: Option<u16> = None;
    let mut rest_id: Option<String> = None;
    // Agents nest by parentage: web's children live under
    // agents/<web>/agents/<child>/agent.json. Walk the whole tree.
    let mut stack = vec![agents_root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            // Descend into a nested `agents/` sub-dir if present.
            let nested = p.join("agents");
            if nested.is_dir() {
                stack.push(nested);
            }
            let agent_json = p.join("agent.json");
            if let Some(v) = read_json(&agent_json) {
                match v.get("handler_module").and_then(Value::as_str) {
                    Some("web.tools") => {
                        if let Some(pp) = v.get("port").and_then(Value::as_u64) {
                            port = Some(pp as u16);
                        }
                    }
                    Some("web_rest.tools") => {
                        if let Some(id) = v.get("id").and_then(Value::as_str) {
                            rest_id = Some(id.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let pid = read_json(&fdir.join("lock.json"))
        .and_then(|v| v.get("pid").and_then(Value::as_u64))
        .map(|p| p as u32);

    Some(Discovered {
        port: port?,
        rest_id: rest_id?,
        pid,
    })
}

fn read_json(p: &Path) -> Option<Value> {
    let s = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    /// Lay down a `.fantastic/` fixture mirroring the REAL nested shape the
    /// substrate writes: store + web(port) under agents/, and web_ws + web_rest
    /// nested under the web agent's own agents/ sub-dir.
    fn seed_fixture(fdir: &Path, with_port: bool, with_lock: bool) {
        let agents = fdir.join("agents");
        let store = agents.join("store");
        fs::create_dir_all(&store).unwrap();
        fs::write(
            store.join("agent.json"),
            json!({
                "id":"store","handler_module":"file_bridge.tools",
                "parent_id":"core","root":".fantastic","ingress_rule":"allow_all"
            })
            .to_string(),
        )
        .unwrap();

        let web = agents.join("web_81b4e3");
        fs::create_dir_all(&web).unwrap();
        let mut web_json = json!({
            "id":"web_81b4e3","handler_module":"web.tools","parent_id":"core"
        });
        if with_port {
            web_json["port"] = json!(8771);
        }
        fs::write(web.join("agent.json"), web_json.to_string()).unwrap();

        // Children nested under web's own agents/ dir.
        let web_ws = web.join("agents").join("web_ws_911f99");
        fs::create_dir_all(&web_ws).unwrap();
        fs::write(
            web_ws.join("agent.json"),
            json!({
                "id":"web_ws_911f99","handler_module":"web_ws.tools",
                "parent_id":"web_81b4e3","ingress_rule":"allow_all"
            })
            .to_string(),
        )
        .unwrap();

        let web_rest = web.join("agents").join("web_rest_95803a");
        fs::create_dir_all(&web_rest).unwrap();
        fs::write(
            web_rest.join("agent.json"),
            json!({
                "id":"web_rest_95803a","handler_module":"web_rest.tools",
                "parent_id":"web_81b4e3","ingress_rule":"allow_all"
            })
            .to_string(),
        )
        .unwrap();

        if with_lock {
            fs::write(fdir.join("lock.json"), json!({"pid":89090}).to_string()).unwrap();
        }
    }

    #[test]
    fn discover_finds_port_rest_and_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let fdir = tmp.path().join(".fantastic");
        seed_fixture(&fdir, true, true);

        let d = Workspace::new(tmp.path())
            .discover()
            .expect("should discover");
        assert_eq!(d.port, 8771);
        assert_eq!(d.rest_id, "web_rest_95803a");
        assert_eq!(d.pid, Some(89090));
    }

    #[test]
    fn discover_without_lock_yields_no_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let fdir = tmp.path().join(".fantastic");
        seed_fixture(&fdir, true, false);

        let d = Workspace::new(tmp.path())
            .discover()
            .expect("should discover");
        assert_eq!(d.port, 8771);
        assert_eq!(d.rest_id, "web_rest_95803a");
        assert_eq!(d.pid, None);
    }

    #[test]
    fn stale_fixture_without_web_port_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let fdir = tmp.path().join(".fantastic");
        // web agent present but no port persisted → not attachable.
        seed_fixture(&fdir, false, true);

        assert!(Workspace::new(tmp.path()).discover().is_none());
    }

    #[test]
    fn discover_on_empty_dir_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Workspace::new(tmp.path()).discover().is_none());
    }

    /// Touch a real file so `is_file()` is true in the resolver tests.
    fn touch(p: &Path) {
        fs::write(p, b"").unwrap();
    }

    #[test]
    fn resolve_env_bin_wins_when_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let env_bin = tmp.path().join("fantastic_kernel");
        touch(&env_bin);
        // A dev candidate also exists, but the env override takes precedence.
        let dev = tmp.path().join("dev_fantastic_kernel");
        touch(&dev);
        let got = resolve_kernel_bin_from(
            Some(env_bin.to_str().unwrap()),
            &[],
            std::slice::from_ref(&dev),
        );
        assert_eq!(got, Some(env_bin));
    }

    #[test]
    fn resolve_env_bin_missing_falls_through_to_path() {
        let tmp = tempfile::tempdir().unwrap();
        let on_path = tmp.path().join("fantastic_kernel");
        touch(&on_path);
        // env points at a non-existent file → ignored; first existing $PATH wins.
        let missing = tmp.path().join("nope");
        let got = resolve_kernel_bin_from(
            Some(missing.to_str().unwrap()),
            std::slice::from_ref(&on_path),
            &[],
        );
        assert_eq!(got, Some(on_path));
    }

    #[test]
    fn resolve_falls_back_to_first_existing_dev_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let release = tmp.path().join("release_kernel"); // does NOT exist
        let debug = tmp.path().join("debug_kernel");
        touch(&debug); // only the debug candidate exists
        let got = resolve_kernel_bin_from(None, &[], &[release, debug.clone()]);
        assert_eq!(got, Some(debug));
    }

    #[test]
    fn resolve_none_when_nothing_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let ghost = tmp.path().join("ghost");
        let got = resolve_kernel_bin_from(
            Some(ghost.to_str().unwrap()),
            std::slice::from_ref(&ghost),
            std::slice::from_ref(&ghost),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn reflect_and_send_urls_are_exact() {
        let base = "http://127.0.0.1:8771";
        let rest = "web_rest_95803a";
        assert_eq!(
            reflect_url(base, rest, None),
            "http://127.0.0.1:8771/web_rest_95803a/_reflect"
        );
        assert_eq!(
            reflect_url(base, rest, Some("core")),
            "http://127.0.0.1:8771/web_rest_95803a/_reflect/core"
        );
        assert_eq!(
            send_url(base, rest, "kernel"),
            "http://127.0.0.1:8771/web_rest_95803a/kernel"
        );
    }

    // ── container backend ────────────────────────────────────────────────

    #[test]
    fn surface_steps_order_and_content() {
        let steps = surface_steps("core", 8800);
        assert_eq!(steps.len(), 4, "exactly the 4 surface steps");

        // 1: store (file_bridge rooted at .fantastic).
        assert_eq!(
            steps[0],
            vec![
                "core",
                "create_agent",
                "handler_module=file_bridge.tools",
                "id=store",
                "root=.fantastic",
                "ingress_rule=allow_all",
            ]
        );
        // 2: web with the port embedded.
        assert_eq!(
            steps[1],
            vec![
                "core",
                "create_agent",
                "handler_module=web.tools",
                "id=web",
                "port=8800",
            ]
        );
        // 3: web_ws under web.
        assert_eq!(
            steps[2],
            vec![
                "web",
                "create_agent",
                "handler_module=web_ws.tools",
                "id=web_ws",
                "ingress_rule=allow_all",
            ]
        );
        // 4: web_rest under web, id=rest (the REST door).
        assert_eq!(
            steps[3],
            vec![
                "web",
                "create_agent",
                "handler_module=web_rest.tools",
                "id=rest",
                "ingress_rule=allow_all",
            ]
        );
        // The python root threads through unchanged.
        assert_eq!(surface_steps("kernel_state", 9)[0][0], "kernel_state");
    }

    #[test]
    fn container_spec_maps_each_runtime() {
        assert_eq!(
            container_spec(Runtime::Rust),
            ("/opt/fantastic/bin/fantastic-rust", "core")
        );
        assert_eq!(
            container_spec(Runtime::Python),
            ("/opt/fantastic/venv/bin/fantastic_kernel", "kernel_state")
        );
        assert_eq!(
            container_spec(Runtime::Swift),
            ("/opt/fantastic/bin/fantastic-swift", "core")
        );
    }

    #[test]
    fn run_daemon_args_podman_has_userns_and_all_flags() {
        let args = run_daemon_args(
            "ft-ws-proj-8800",
            8800,
            "/abs/work",
            "rust",
            "fantastic:arm64",
            true,
        );
        assert!(args.contains(&"-d".to_string()));
        // --name <name> (adjacent).
        let ni = args.iter().position(|a| a == "--name").unwrap();
        assert_eq!(args[ni + 1], "ft-ws-proj-8800");
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"127.0.0.1:8800:8800".to_string()));
        assert!(args.contains(&"-v".to_string()));
        assert!(args.contains(&"/abs/work:/work".to_string()));
        assert!(args.contains(&"FANTASTIC_RUNTIME=rust".to_string()));
        assert!(args.contains(&"FANTASTIC_PORT=8800".to_string()));
        assert!(args.contains(&"fantastic:arm64".to_string()));
        assert!(
            args.contains(&"--userns=keep-id".to_string()),
            "podman carries --userns=keep-id"
        );
        // The image is the LAST arg (no trailing daemon args here).
        assert_eq!(args.last().unwrap(), "fantastic:arm64");
    }

    #[test]
    fn run_daemon_args_docker_omits_userns() {
        let args = run_daemon_args(
            "ft-ws-proj-8800",
            8800,
            "/abs/work",
            "python",
            "fantastic:arm64",
            false,
        );
        assert!(
            !args.contains(&"--userns=keep-id".to_string()),
            "docker must NOT carry --userns=keep-id"
        );
        assert!(args.contains(&"FANTASTIC_RUNTIME=python".to_string()));
    }

    #[test]
    fn compose_args_replays_script_via_entrypoint_sh() {
        let args = compose_args("/abs/work", "fantastic:arm64", true, "bin a && bin b");
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--rm".to_string()));
        assert!(args.contains(&"--userns=keep-id".to_string()));
        assert!(args.contains(&"/abs/work:/work".to_string()));
        assert!(args.contains(&"--entrypoint".to_string()));
        assert!(args.contains(&"sh".to_string()));
        assert!(args.contains(&"-c".to_string()));
        assert_eq!(args.last().unwrap(), "bin a && bin b");
        // docker variant drops --userns.
        let d = compose_args("/abs/work", "img", false, "x");
        assert!(!d.contains(&"--userns=keep-id".to_string()));
    }

    #[test]
    fn sanitize_name_lowers_and_strips() {
        assert_eq!(sanitize_name("My Proj.dir"), "my-proj-dir");
        assert_eq!(sanitize_name("--weird--"), "weird");
        assert_eq!(sanitize_name("///"), "ws");
    }
}
