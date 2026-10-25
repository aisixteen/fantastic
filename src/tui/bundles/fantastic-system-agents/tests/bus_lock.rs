//! `sys::linux::lock` against a fake logind (python-dbusmock's `logind`
//! template, with `GetUserByPID` added and the user's `Display` set), through
//! the real bundle in a real in-memory kernel. `<home>` is a temp dir.

#![cfg(target_os = "linux")]

#[path = "support/bus.rs"]
mod bus;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system_agents::lock::{LockBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::linux::LinuxLock;
use serde_json::{json, Value};
use zbus::zvariant::{ObjectPath, OwnedValue, StructureBuilder, Value as V};

const MOCK: &str = "org.freedesktop.DBus.Mock";
const LOGIND: &str = "org.freedesktop.login1";
const MANAGER: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";
const USER: &str = "/org/freedesktop/login1/user/1000";
const PIN: &str = "2468";

fn session(id: &str) -> String {
    format!("/org/freedesktop/login1/session/{id}")
}

/// A temp `<home>`, removed on drop.
struct Home(PathBuf);

impl Home {
    fn new(name: &str) -> Home {
        let dir =
            std::env::temp_dir().join(format!("fantastic_lock_bus_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Home(dir)
    }
}

impl Drop for Home {
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
        last = op(k, "lock", json!({"type": "state"})).await;
        if ok(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never saw: {what}; the last state: {last}");
}

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

/// Make the user's `Display` name session `id`.
async fn display(conn: &zbus::Connection, id: &str) {
    let d = StructureBuilder::new()
        .add_field(id)
        .add_field(ObjectPath::try_from(session(id)).unwrap())
        .build()
        .unwrap();
    let mut props: HashMap<&str, V<'_>> = HashMap::new();
    props.insert("Display", V::Structure(d));
    mock(
        conn,
        USER,
        "UpdateProperties",
        &("org.freedesktop.login1.User", props),
    )
    .await;
}

/// The logind calls on a session object's mock method `name`: each call's arguments.
async fn calls(conn: &zbus::Connection, path: &str, name: &str) -> Vec<Vec<OwnedValue>> {
    let r = mock(conn, path, "GetMethodCalls", &(name,)).await;
    let all: Vec<(u64, Vec<OwnedValue>)> = r.body().deserialize().expect("a(tav)");
    all.into_iter().map(|(_, a)| a).collect()
}

/// `ListInhibitors`: how many sleep inhibitors are held.
async fn inhibitors(conn: &zbus::Connection) -> usize {
    let r = conn
        .call_method(
            Some(LOGIND),
            MANAGER,
            Some(MANAGER_IFACE),
            "ListInhibitors",
            &(),
        )
        .await
        .expect("ListInhibitors");
    let list: Vec<(String, String, String, String, u32, u32)> = r.body().deserialize().unwrap();
    list.iter()
        .filter(|i| i.0 == "sleep" && i.1 == "fantastic")
        .count()
}

struct World {
    _bus: bus::Bus,
    conn: zbus::Connection,
    _home: Home,
    k: Arc<Kernel>,
}

/// logind with the kiosk user (uid 1000) in session `c1`, its `Display`; the
/// agent on it with the PIN `2468` set.
async fn world(name: &str) -> World {
    let bus = bus::Bus::start(&[("logind", None)]).await;
    let conn = bus.connect().await;
    mock(
        &conn,
        MANAGER,
        "AddSession",
        &("c1", "seat0", 1000u32, "fantastic", true),
    )
    .await;
    mock(
        &conn,
        MANAGER,
        "AddMethod",
        &(
            MANAGER_IFACE,
            "GetUserByPID",
            "u",
            "o",
            format!("ret = '{USER}'"),
        ),
    )
    .await;
    display(&conn, "c1").await;
    let home = Home::new(name);
    let mut reg = BundleRegistry::new();
    reg.register(
        HANDLER_MODULE,
        LockBundle::with_port(Arc::new(LinuxLock::with_home(bus.connect().await, &home.0))),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "lock", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    state_where(&k, "available", |s| s["available"] == true).await;
    let r = op(&k, "lock", json!({"type": "set_pin", "new": PIN})).await;
    assert_eq!(r["has_pin"], true, "{r}");
    // the PIN is in lock.yaml, hashed, private
    let text = std::fs::read_to_string(home.0.join("lock.yaml")).unwrap();
    assert!(text.contains("$argon2id$") && !text.contains(PIN), "{text}");
    World {
        _bus: bus,
        conn,
        _home: home,
        k,
    }
}

async fn session_call(conn: &zbus::Connection, id: &str, method: &str) {
    conn.call_method(
        Some(LOGIND),
        session(id).as_str(),
        Some(SESSION_IFACE),
        method,
        &(),
    )
    .await
    .unwrap_or_else(|e| panic!("{method}: {e}"));
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn an_emitted_lock_locks() {
    let w = world("lock").await;
    // `loginctl lock-session` is the session's `Lock` signal
    session_call(&w.conn, "c1", "Lock").await;
    let s = state_where(&w.k, "locked", |s| s["locked"] == true).await;
    assert_eq!(s["has_pin"], true, "{s}");
    // and the agent mirrors it into the session's LockedHint
    let hints = calls(&w.conn, &session("c1"), "SetLockedHint").await;
    let last = bool::try_from(&hints.last().expect("SetLockedHint was called")[0]).unwrap();
    assert!(last, "{hints:?}");
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn unlock_with_the_right_pin_unlocks_and_calls_set_locked_hint_false() {
    let w = world("unlock").await;
    session_call(&w.conn, "c1", "Lock").await;
    state_where(&w.k, "locked", |s| s["locked"] == true).await;
    let r = op(&w.k, "lock", json!({"type": "unlock", "pin": "0000"})).await;
    assert_eq!(r["reason"], "wrong_pin", "{r}");
    assert_eq!(r["failures"], Value::Null, "a refusal is not a state: {r}");
    let r = op(&w.k, "lock", json!({"type": "unlock", "pin": PIN})).await;
    assert_eq!(
        (r["locked"].clone(), r["failures"].clone()),
        (json!(false), json!(0)),
        "{r}"
    );
    let hints = calls(&w.conn, &session("c1"), "SetLockedHint").await;
    let last = bool::try_from(&hints.last().unwrap()[0]).unwrap();
    assert!(!last, "the last SetLockedHint is false: {hints:?}");
    // the PIN survives a restart of the agent: a new one reads has_pin from lock.yaml
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn five_wrong_pins_set_retry_at_ms() {
    let w = world("wrong").await;
    let r = op(&w.k, "lock", json!({"type": "lock"})).await;
    assert_eq!(r["locked"], true, "{r}");
    for n in 1..=4 {
        let r = op(&w.k, "lock", json!({"type": "unlock", "pin": "1111"})).await;
        assert_eq!(r["reason"], "wrong_pin", "try {n}: {r}");
    }
    let s = op(&w.k, "lock", json!({"type": "state"})).await;
    assert_eq!(
        (s["failures"].clone(), s["retry_at_ms"].clone()),
        (json!(4), Value::Null)
    );
    let r = op(&w.k, "lock", json!({"type": "unlock", "pin": "1111"})).await;
    assert_eq!(r["reason"], "wrong_pin", "{r}");
    let s = op(&w.k, "lock", json!({"type": "state"})).await;
    assert_eq!(s["failures"], 5, "{s}");
    let retry = s["retry_at_ms"].as_u64().expect("retry_at_ms is set");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(
        retry > now && retry <= now + 31_000,
        "30 s from now: {retry} vs {now}"
    );
    // even the right PIN waits
    let r = op(&w.k, "lock", json!({"type": "unlock", "pin": PIN})).await;
    assert_eq!(r["reason"], "retry_later", "{r}");
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn prepare_for_sleep_true_locks_and_releases_the_inhibitor() {
    let w = world("sleep").await;
    assert_eq!(
        inhibitors(&w.conn).await,
        1,
        "the agent holds a sleep inhibitor"
    );
    let emit = |start: bool| {
        let conn = w.conn.clone();
        async move {
            mock(
                &conn,
                MANAGER,
                "EmitSignal",
                &(MANAGER_IFACE, "PrepareForSleep", "b", vec![V::Bool(start)]),
            )
            .await;
        }
    };
    emit(true).await;
    state_where(&w.k, "locked for the sleep", |s| s["locked"] == true).await;
    for _ in 0..100 {
        if inhibitors(&w.conn).await == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(inhibitors(&w.conn).await, 0, "released, so sleep proceeds");
    // waking takes a new one
    emit(false).await;
    for _ in 0..100 {
        if inhibitors(&w.conn).await == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no new inhibitor after PrepareForSleep(false)");
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn a_new_display_moves_the_subscription() {
    let w = world("display").await;
    mock(
        &w.conn,
        MANAGER,
        "AddSession",
        &("c2", "seat0", 1000u32, "fantastic", true),
    )
    .await;
    display(&w.conn, "c2").await;
    // a Lock on the new session locks (the signal is sent until the agent has moved)
    let mut locked = false;
    for _ in 0..60 {
        session_call(&w.conn, "c2", "Lock").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        if op(&w.k, "lock", json!({"type": "state"})).await["locked"] == true {
            locked = true;
            break;
        }
    }
    assert!(
        locked,
        "a Lock on the new display session never reached the agent"
    );
    let r = op(&w.k, "lock", json!({"type": "unlock", "pin": PIN})).await;
    assert_eq!(r["locked"], false, "{r}");
    // the old session is no longer followed
    session_call(&w.conn, "c1", "Lock").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = op(&w.k, "lock", json!({"type": "state"})).await;
    assert_eq!(
        s["locked"], false,
        "a Lock on the old session is ignored: {s}"
    );
}
