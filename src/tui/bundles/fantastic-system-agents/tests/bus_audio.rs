//! `sys::linux::audio` against fake PipeWire tools: shell scripts stand in for
//! `pw-dump` (prints a dump, or `--monitor` arrays) and `wpctl` (logs its
//! argv), run by the real port and, at the end, through the real bundle in a
//! real in-memory kernel. No PipeWire and no bus needed.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system::audio::{Call, Event};
use fantastic_system_agents::audio::{AudioBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::linux::LinuxAudio;
use fantastic_system_agents::sys::{AudioOs, OsError};
use serde_json::{json, Value};

const DUMP: &str = include_str!("../../../crates/fantastic-system/tests/fixtures/pw_dump.json");

/// A scratch dir with the fake tools.
struct Tools {
    dir: PathBuf,
}

impl Tools {
    /// `monitor` is what `pw-dump --monitor` does (shell).
    fn new(name: &str, monitor: &str, wpctl: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fc-bus-audio-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dump.json"), DUMP).unwrap();
        let d = dir.display();
        script(
            &dir.join("pw-dump"),
            &format!("if [ \"$1\" = --monitor ]; then\n{monitor}\nfi\ncat '{d}/dump.json'\n"),
        );
        script(
            &dir.join("wpctl"),
            &format!("echo \"$@\" >> '{d}/wpctl.log'\n{wpctl}\n"),
        );
        // The probes above logged through the fake wpctl.
        let _ = std::fs::remove_file(dir.join("wpctl.log"));
        Tools { dir }
    }

    fn port(&self) -> LinuxAudio {
        LinuxAudio::with_programs(self.dir.join("pw-dump"), self.dir.join("wpctl"))
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("wpctl.log")).unwrap_or_default()
    }
}

impl Drop for Tools {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A sibling test's fork can hold our write fd until its exec, so a run
    // right now may get ETXTBSY. Once a probe execs, no holder is left.
    for _ in 0..200 {
        match std::process::Command::new(path)
            .arg("--probe")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(10)),
            _ => return,
        }
    }
}

/// Two `--monitor` arrays, then quiet.
const TWO_THEN_SLEEP: &str =
    "echo '[{\"id\": 52, \"info\": null}]'\necho '[{\"id\": 53, \"info\": null}]'\nexec sleep 30";

async fn next_delta(rx: &mut tokio::sync::mpsc::Receiver<Event>, within: Duration) -> Value {
    match tokio::time::timeout(within, rx.recv()).await {
        Ok(Some(Event::Delta(v))) => v,
        other => panic!("no delta: {other:?}"),
    }
}

#[tokio::test]
async fn read_runs_pw_dump_and_parses_its_json() {
    let t = Tools::new("read", TWO_THEN_SLEEP, "");
    let v = t.port().read().await.expect("read");
    let want: Value = serde_json::from_str(DUMP).unwrap();
    assert_eq!(v, want);
}

#[tokio::test]
async fn set_volume_runs_wpctl_set_volume_with_the_limit() {
    let t = Tools::new("volume", TWO_THEN_SLEEP, "");
    t.port()
        .call(Call::SetVolume {
            id: 52,
            volume: 0.3,
        })
        .await
        .expect("call");
    assert_eq!(t.log(), "set-volume 52 0.30 -l 1.5\n");
}

#[tokio::test]
async fn set_mute_and_set_default_run_wpctl_with_the_documented_argv() {
    let t = Tools::new("mute", TWO_THEN_SLEEP, "");
    let port = t.port();
    port.call(Call::SetMute {
        id: 52,
        muted: true,
    })
    .await
    .expect("mute");
    port.call(Call::SetDefault { id: 53 })
        .await
        .expect("default");
    assert_eq!(t.log(), "set-mute 52 1\nset-default 53\n");
}

#[tokio::test]
async fn a_failing_wpctl_is_a_failed_error_with_its_stderr() {
    let t = Tools::new(
        "fail",
        TWO_THEN_SLEEP,
        "echo 'Object 52 not found' >&2\nexit 1",
    );
    let e = t
        .port()
        .call(Call::SetDefault { id: 52 })
        .await
        .unwrap_err();
    assert_eq!(e, OsError::Failed("Object 52 not found".into()));
}

#[tokio::test]
async fn a_missing_wpctl_is_unavailable_with_wpctl_is_not_installed() {
    let t = Tools::new("missing", TWO_THEN_SLEEP, "");
    let port = LinuxAudio::with_programs(t.dir.join("pw-dump"), t.dir.join("no-wpctl"));
    let why = OsError::Unavailable("wpctl is not installed".into());
    assert_eq!(port.read().await.unwrap_err(), why);
    assert_eq!(
        port.call(Call::SetDefault { id: 52 }).await.unwrap_err(),
        why
    );
    let port = LinuxAudio::with_programs(t.dir.join("no-pw-dump"), t.dir.join("wpctl"));
    assert_eq!(
        port.read().await.unwrap_err(),
        OsError::Unavailable("pw-dump is not installed".into())
    );
}

#[tokio::test]
async fn monitor_output_arrives_as_delta_events() {
    let t = Tools::new("monitor", TWO_THEN_SLEEP, "");
    let port = t.port();
    let mut rx = port.changes();
    let within = Duration::from_secs(5);
    assert_eq!(
        next_delta(&mut rx, within).await,
        json!([{"id": 52, "info": null}])
    );
    assert_eq!(
        next_delta(&mut rx, within).await,
        json!([{"id": 53, "info": null}])
    );
}

#[tokio::test]
async fn the_monitor_child_is_restarted_when_it_exits() {
    // one array, then the child ends: the port starts it again after 1 s
    let t = Tools::new(
        "restart",
        "echo '[{\"id\": 7, \"info\": null}]'\nexit 0",
        "",
    );
    let port = t.port();
    let mut rx = port.changes();
    let within = Duration::from_secs(5);
    let first = next_delta(&mut rx, within).await;
    let second = next_delta(&mut rx, within).await;
    assert_eq!(first, second, "the same program, run again");
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

#[tokio::test(flavor = "multi_thread")]
async fn an_end_to_end_agent_over_the_real_port_with_fakes() {
    let t = Tools::new("e2e", "exec sleep 30", "");
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, AudioBundle::with_port(Arc::new(t.port())));
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "audio", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    let mut s = Value::Null;
    for _ in 0..200 {
        s = op(&k, "audio", json!({"type": "state"})).await;
        if s["available"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(s["available"], true, "{s}");
    assert_eq!(s["sinks"][0]["id"], 52);
    assert_eq!(s["sinks"][0]["description"], "Built-in Audio Analog Stereo");
    let r = op(&k, "audio", json!({"type": "set_volume", "volume": 0.3})).await;
    assert_eq!(r["available"], true, "the reply is the state: {r}");
    assert_eq!(t.log(), "set-volume 52 0.30 -l 1.5\n");
}
