//! `sys::linux::power` against a fake bus: python-dbusmock's `upower` and
//! `logind` templates on one private daemon, driven through the real bundle in
//! a real in-memory kernel.

#![cfg(target_os = "linux")]

#[path = "support/bus.rs"]
mod bus;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system_agents::power::{PowerBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::linux::LinuxPower;
use serde_json::{json, Value};
use zbus::zvariant::{ObjectPath, OwnedValue, Value as V};

const MOCK: &str = "org.freedesktop.DBus.Mock";
const DISPLAY: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn state_where(k: &Arc<Kernel>, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        let s = op(k, "power", json!({"type": "state"})).await;
        if ok(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never saw: {what}");
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn power_reads_follows_and_acts_on_a_fake_bus() {
    let bus = bus::Bus::start(&[("upower", None), ("logind", None)]).await;
    let conn = bus.connect().await;
    // a 50 % discharging battery
    conn.call_method(
        Some("org.freedesktop.UPower"),
        "/org/freedesktop/UPower",
        Some(MOCK),
        "SetupDisplayDevice",
        &(
            2u32,
            2u32,
            50.0f64,
            25.0f64,
            50.0f64,
            10.0f64,
            7200i64,
            0i64,
            true,
            "battery-good",
            1u32,
        ),
    )
    .await
    .expect("SetupDisplayDevice");

    let mut reg = BundleRegistry::new();
    reg.register(
        HANDLER_MODULE,
        PowerBundle::with_port(Arc::new(LinuxPower::new(bus.connect().await))),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "power", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");

    let s = state_where(&k, "the 50 % battery", |s| s["battery"]["percent"] == 50.0).await;
    assert_eq!(s["battery"]["state"], "discharging", "{s}");
    assert_eq!(s["can"]["suspend"], "yes", "{s}");

    // a change on the bus reaches a watcher as a `state` event within a second
    let w = AgentId::from("watcher");
    let mut rx = k.ensure_inbox(&w);
    assert!(k.watch(&AgentId::from("power"), w).await.is_none());
    let mut props: HashMap<&str, V<'_>> = HashMap::new();
    props.insert("Percentage", V::F64(40.0));
    conn.call_method(
        Some("org.freedesktop.UPower"),
        "/org/freedesktop/UPower",
        Some(MOCK),
        "SetDeviceProperties",
        &(ObjectPath::try_from(DISPLAY).unwrap(), props),
    )
    .await
    .expect("SetDeviceProperties");
    let start = std::time::Instant::now();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("a state event within 1 s")
            .expect("inbox open");
        if item.payload["type"] == "state" && item.payload["state"]["battery"]["percent"] == 40.0 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "no 40 % state within 1 s"
        );
    }

    // suspend reaches logind as Suspend(false)
    let r = op(&k, "power", json!({"type": "suspend"})).await;
    assert!(r.get("error").is_none(), "{r}");
    let calls = conn
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some(MOCK),
            "GetMethodCalls",
            &("Suspend",),
        )
        .await
        .expect("GetMethodCalls");
    let calls: Vec<(u64, Vec<OwnedValue>)> = calls.body().deserialize().expect("a(tav)");
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        !bool::try_from(&calls[0].1[0]).unwrap(),
        "interactive is false"
    );

    // CanSuspend = na: the next read refuses suspend
    conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some(MOCK),
        "AddMethod",
        &(
            "org.freedesktop.login1.Manager",
            "CanSuspend",
            "",
            "s",
            "ret = 'na'",
        ),
    )
    .await
    .expect("AddMethod");
    let mut props: HashMap<&str, V<'_>> = HashMap::new();
    props.insert("Percentage", V::F64(39.0));
    conn.call_method(
        Some("org.freedesktop.UPower"),
        "/org/freedesktop/UPower",
        Some(MOCK),
        "SetDeviceProperties",
        &(ObjectPath::try_from(DISPLAY).unwrap(), props),
    )
    .await
    .expect("SetDeviceProperties");
    state_where(&k, "can.suspend = na", |s| s["can"]["suspend"] == "na").await;
    let r = op(&k, "power", json!({"type": "suspend"})).await;
    assert_eq!(r["reason"], "unsupported", "{r}");
}
