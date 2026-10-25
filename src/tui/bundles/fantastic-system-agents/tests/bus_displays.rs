//! `sys::linux::displays` against a fake logind (python-dbusmock's `logind`
//! template, with `GetUserByPID` and `SetBrightness` added) and a fake
//! `wlr-randr` script that records its argv, through the real bundle in a real
//! in-memory kernel. The Wayland socket is a file in a temp directory and the
//! backlight a temp directory in the shape of `/sys/class/backlight`.

#![cfg(target_os = "linux")]

#[path = "support/bus.rs"]
mod bus;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system_agents::displays::{DisplaysBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::linux::LinuxDisplays;
use serde_json::{json, Value};
use zbus::zvariant::{ObjectPath, OwnedValue, StructureBuilder, Value as V};

const MOCK: &str = "org.freedesktop.DBus.Mock";
const LOGIND: &str = "org.freedesktop.login1";
const MANAGER: &str = "/org/freedesktop/login1";
const USER: &str = "/org/freedesktop/login1/user/1000";
const SESSION: &str = "/org/freedesktop/login1/session/c1";

const OUTPUTS: &str = r#"[{"name": "Virtual-1", "enabled": true, "make": "", "model": "",
  "modes": [{"width": 1280, "height": 800, "refresh": 60.0, "preferred": true, "current": true}],
  "position": {"x": 0, "y": 0}, "transform": "normal", "scale": 1.0}]"#;

/// A temp directory of the test, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("fantastic_displays_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn path(&self, p: &str) -> PathBuf {
        self.0.join(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn state_where(k: &Arc<Kernel>, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    for _ in 0..200 {
        last = op(k, "displays", json!({"type": "state"})).await;
        if ok(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never saw: {what}; the last state: {last}");
}

/// Call a mock method on logind.
async fn mock(
    conn: &zbus::Connection,
    path: &str,
    member: &str,
    body: &(impl zbus::export::serde::Serialize + zbus::zvariant::DynamicType),
) -> zbus::Message {
    conn.call_method(Some(LOGIND), path, Some(MOCK), member, body)
        .await
        .unwrap_or_else(|e| panic!("{member}: {e}"))
}

/// logind with the kiosk user `fantastic` (uid 1000) in session `c1`, which is
/// the user's `Display`; `SetBrightness` writes its value to `brightness_file`.
async fn logind(conn: &zbus::Connection, brightness_file: &Path) {
    mock(
        conn,
        MANAGER,
        "AddSession",
        &("c1", "seat0", 1000u32, "fantastic", true),
    )
    .await;
    mock(
        conn,
        MANAGER,
        "AddMethod",
        &(
            "org.freedesktop.login1.Manager",
            "GetUserByPID",
            "u",
            "o",
            format!("ret = '{USER}'"),
        ),
    )
    .await;
    let display = StructureBuilder::new()
        .add_field("c1")
        .add_field(ObjectPath::try_from(SESSION).unwrap())
        .build()
        .unwrap();
    let mut props: HashMap<&str, V<'_>> = HashMap::new();
    props.insert("Display", V::Structure(display));
    mock(
        conn,
        USER,
        "UpdateProperties",
        &("org.freedesktop.login1.User", props),
    )
    .await;
    mock(
        conn,
        SESSION,
        "AddMethod",
        &(
            "org.freedesktop.login1.Session",
            "SetBrightness",
            "ssu",
            "",
            format!(
                "open('{}', 'w').write(str(args[2]))",
                brightness_file.display()
            ),
        ),
    )
    .await;
}

/// A `wlr-randr` that logs its arguments, one call a line, and prints `OUTPUTS` for `--json`.
fn fake_wlr_randr(dir: &Scratch) -> (PathBuf, PathBuf) {
    let log = dir.path("wlr-randr.log");
    let outputs = dir.path("outputs.json");
    std::fs::write(&outputs, OUTPUTS).unwrap();
    let script = dir.path("wlr-randr");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nif [ \"$1\" = \"--json\" ]; then cat '{}'; fi\n",
            log.display(),
            outputs.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

/// A backlight device directory under `root`.
fn backlight(root: &Path, name: &str, brightness: u32, max: u32) -> PathBuf {
    let d = root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("brightness"), brightness.to_string()).unwrap();
    std::fs::write(d.join("max_brightness"), max.to_string()).unwrap();
    std::fs::write(d.join("type"), "raw\n").unwrap();
    d.join("brightness")
}

/// The agent over `port`.
async fn agent(port: LinuxDisplays) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, DisplaysBundle::with_port(Arc::new(port)));
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "displays", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    k
}

/// Everything a test needs: the bus, the scratch directory, the fakes and the agent.
struct World {
    _bus: bus::Bus,
    conn: zbus::Connection,
    dir: Scratch,
    log: PathBuf,
    k: Arc<Kernel>,
}

/// `with_backlight`: a device `intel_backlight` at 300 of 1000; `socket`: the
/// Wayland socket exists; `program`: the fake `wlr-randr` is installed.
async fn world(name: &str, with_backlight: bool, socket: bool, program: bool) -> World {
    let bus = bus::Bus::start(&[("logind", None)]).await;
    let conn = bus.connect().await;
    let dir = Scratch::new(name);
    let lights = dir.path("backlight");
    std::fs::create_dir_all(&lights).unwrap();
    let file = if with_backlight {
        backlight(&lights, "intel_backlight", 300, 1000)
    } else {
        lights.join("nothing")
    };
    logind(&conn, &file).await;
    let run = dir.path("run");
    std::fs::create_dir_all(&run).unwrap();
    if socket {
        std::fs::write(run.join("wayland-0"), "").unwrap();
    }
    let (script, log) = fake_wlr_randr(&dir);
    let program = if program {
        script
    } else {
        dir.path("not-installed")
    };
    let port = LinuxDisplays::new(bus.connect().await)
        .with_program(program)
        .with_wayland(run, "wayland-0")
        .with_backlight_dir(if with_backlight {
            lights
        } else {
            dir.path("no-such-dir")
        });
    let k = agent(port).await;
    World {
        _bus: bus,
        conn,
        dir,
        log,
        k,
    }
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn set_output_runs_wlr_randr_with_the_argv() {
    let w = world("set_output", true, true, true).await;
    let s = state_where(&w.k, "the output", |s| {
        s["outputs"][0]["name"] == "Virtual-1"
    })
    .await;
    assert_eq!(s["outputs"][0]["mode"]["width"], 1280, "{s}");
    let r = op(
        &w.k,
        "displays",
        json!({"type": "set_output", "name": "Virtual-1", "scale": 2, "transform": "180", "position": [10, 20]}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    let log = std::fs::read_to_string(&w.log).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines.contains(&"--json"), "the read: {lines:?}");
    assert!(
        lines.contains(&"--output Virtual-1 --scale 2 --transform 180 --pos 10,20"),
        "the write: {lines:?}"
    );
    // the last enabled output stays on: the script is not run for it
    let r = op(
        &w.k,
        "displays",
        json!({"type": "set_output", "name": "Virtual-1", "enabled": false}),
    )
    .await;
    assert_eq!(r["reason"], "unsupported", "{r}");
    assert!(!std::fs::read_to_string(&w.log).unwrap().contains("--off"));
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn set_brightness_calls_logind_on_the_display_session_and_reads_back() {
    let w = world("brightness", true, true, true).await;
    let s = state_where(&w.k, "the backlight", |s| s["backlight"]["value"] == 0.3).await;
    assert_eq!(s["backlight"]["name"], "intel_backlight");
    let r = op(
        &w.k,
        "displays",
        json!({"type": "set_brightness", "value": 0.8}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    // logind was called on the session `Display` names, with the device and the level
    let calls = w
        .conn
        .call_method(
            Some(LOGIND),
            SESSION,
            Some(MOCK),
            "GetMethodCalls",
            &("SetBrightness",),
        )
        .await
        .expect("GetMethodCalls");
    let calls: Vec<(u64, Vec<OwnedValue>)> = calls.body().deserialize().expect("a(tav)");
    assert_eq!(calls.len(), 1, "{calls:?}");
    let args = &calls[0].1;
    assert_eq!(
        String::try_from(args[0].try_clone().unwrap()).unwrap(),
        "backlight"
    );
    assert_eq!(
        String::try_from(args[1].try_clone().unwrap()).unwrap(),
        "intel_backlight"
    );
    assert_eq!(u32::try_from(args[2].try_clone().unwrap()).unwrap(), 800);
    // the fake wrote the level to the device file, and the state reads it back
    state_where(&w.k, "0.8 read back", |s| s["backlight"]["value"] == 0.8).await;
    let r = op(
        &w.k,
        "displays",
        json!({"type": "set_brightness", "value": 9}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    state_where(&w.k, "clamped to 1.0", |s| s["backlight"]["value"] == 1.0).await;
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn backlight_fallback_when_no_backlight_dir() {
    let w = world("no_backlight", false, true, true).await;
    let s = state_where(&w.k, "read", |s| s["available"] == true).await;
    assert!(s["backlight"].is_null(), "{s}");
    let r = op(
        &w.k,
        "displays",
        json!({"type": "set_brightness", "value": 0.5}),
    )
    .await;
    assert_eq!(r["reason"], "unsupported", "{r}");
    drop(w.dir);
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_wlr_randr_is_unavailable_with_why() {
    let w = world("no_program", true, true, false).await;
    let s = state_where(&w.k, "unavailable", |s| {
        s["available"] == false && s["why"] != "not read yet"
    })
    .await;
    assert_eq!(s["why"], "wlr-randr is not installed");
    let r = op(
        &w.k,
        "displays",
        json!({"type": "set_output", "name": "Virtual-1", "scale": 2}),
    )
    .await;
    assert_eq!(r["reason"], "unavailable", "{r}");
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_wayland_socket_says_no_wayland_display() {
    let w = world("no_socket", true, false, true).await;
    let s = state_where(&w.k, "unavailable", |s| {
        s["available"] == false && s["why"] != "not read yet"
    })
    .await;
    let want = format!(
        "no Wayland display at {}",
        w.dir.path("run/wayland-0").display()
    );
    assert_eq!(s["why"], want.as_str());
    assert!(!w.log.exists(), "wlr-randr was not run without a socket");
}
