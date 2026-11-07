//! `sys::linux::notifications` against a private session bus (a plain
//! `dbus-daemon`, no template: the agent is the server), driven through the
//! real bundle in a real in-memory kernel, with a zbus client as the program
//! that notifies.

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
use fantastic_system_agents::notifications::{NotificationsBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::linux::LinuxNotifications;
use futures_util::StreamExt;
use serde_json::{json, Value};
use zbus::zvariant::Value as V;

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn state_where(k: &Arc<Kernel>, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        let s = op(k, "notifications", json!({"type": "state"})).await;
        if ok(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never saw: {what}");
}

/// The agent, on a connection of the bus.
async fn agent(bus: &bus::Bus) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(
        HANDLER_MODULE,
        NotificationsBundle::with_port(Arc::new(LinuxNotifications::new(bus.connect().await))),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "notifications", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    k
}

/// A client's `Notify`: the id it gets back.
async fn notify(
    client: &zbus::Connection,
    replaces_id: u32,
    summary: &str,
    actions: Vec<&str>,
    hints: HashMap<&str, V<'_>>,
) -> u32 {
    let r = client
        .call_method(
            Some(NAME),
            PATH,
            Some(NAME),
            "Notify",
            &(
                "test-app",
                replaces_id,
                "",
                summary,
                "the body",
                actions,
                hints,
                -1i32,
            ),
        )
        .await
        .expect("Notify");
    r.body().deserialize().expect("a u32")
}

/// The signals `member` of the notification server, as a stream on `client`.
async fn signals(client: &zbus::Connection, member: &'static str) -> zbus::MessageStream {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(NAME)
        .unwrap()
        .member(member)
        .unwrap()
        .build();
    zbus::MessageStream::for_match_rule(rule, client, None)
        .await
        .expect("a signal stream")
}

async fn next_signal<T>(stream: &mut zbus::MessageStream) -> T
where
    T: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let msg = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("the signal within 3 s")
        .expect("open")
        .expect("a message");
    msg.body().deserialize().expect("the signal's body")
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn the_agent_owns_the_name_stores_notify_and_emits_closed_and_action() {
    let bus = bus::Bus::start(&[]).await;
    let k = agent(&bus).await;
    let s = state_where(&k, "the name owned", |s| s["server"] == "owned").await;
    assert_eq!(s["available"], true, "{s}");
    let client = bus.connect().await;

    // GetCapabilities and GetServerInformation answer as documented
    let caps: Vec<String> = client
        .call_method(Some(NAME), PATH, Some(NAME), "GetCapabilities", &())
        .await
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    assert_eq!(caps, vec!["body", "actions", "persistence"]);
    let info: (String, String, String, String) = client
        .call_method(Some(NAME), PATH, Some(NAME), "GetServerInformation", &())
        .await
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    assert_eq!(
        (info.0.as_str(), info.1.as_str(), info.3.as_str()),
        ("fantastic", "aisixteen", "1.2")
    );

    // a client's Notify lands in the state, and a state event follows
    let w = AgentId::from("watcher");
    let mut rx = k.ensure_inbox(&w);
    assert!(k.watch(&AgentId::from("notifications"), w).await.is_none());
    let mut hints = HashMap::new();
    hints.insert("urgency", V::U8(1));
    hints.insert("desktop-entry", V::from("org.example.App"));
    hints.insert("ignored", V::U8(9));
    let id = notify(&client, 0, "hello", vec!["default", "Open"], hints).await;
    assert_eq!(id, 1);
    let mut seen = false;
    for _ in 0..40 {
        let item = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("a state event")
            .expect("inbox open");
        if item.payload["type"] == "state"
            && item.payload["state"]["items"][0]["summary"] == "hello"
        {
            let i = &item.payload["state"]["items"][0];
            assert_eq!(
                (i["app"].as_str(), i["desktop_entry"].as_str()),
                (Some("test-app"), Some("org.example.App"))
            );
            seen = true;
            break;
        }
    }
    assert!(seen, "no state event with the notification");

    // replaces_id replaces it in place: same id, one item
    let again = notify(&client, id, "hello again", vec![], HashMap::new()).await;
    assert_eq!(again, id);
    let s = state_where(&k, "replaced", |s| {
        s["items"][0]["summary"] == "hello again"
    })
    .await;
    assert_eq!(s["items"].as_array().unwrap().len(), 1, "{s}");

    // CloseNotification emits NotificationClosed(id, 3)
    let mut closed = signals(&client, "NotificationClosed").await;
    client
        .call_method(Some(NAME), PATH, Some(NAME), "CloseNotification", &(id,))
        .await
        .expect("CloseNotification");
    assert_eq!(next_signal::<(u32, u32)>(&mut closed).await, (id, 3));
    state_where(&k, "closed", |s| s["items"].as_array().unwrap().is_empty()).await;

    // invoke emits ActionInvoked, and a resident item stays
    let mut invoked = signals(&client, "ActionInvoked").await;
    let mut hints = HashMap::new();
    hints.insert("resident", V::Bool(true));
    let sticky = notify(&client, 0, "sticky", vec!["default", "Open"], hints).await;
    let plain = notify(&client, 0, "plain", vec!["default", "Open"], HashMap::new()).await;
    let r = op(
        &k,
        "notifications",
        json!({"type": "invoke", "id": sticky, "action": "default"}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(
        next_signal::<(u32, String)>(&mut invoked).await,
        (sticky, "default".to_string())
    );
    assert!(
        r["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == sticky),
        "resident stays: {r}"
    );
    let r = op(
        &k,
        "notifications",
        json!({"type": "invoke", "id": plain, "action": "default"}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(
        next_signal::<(u32, String)>(&mut invoked).await,
        (plain, "default".to_string())
    );
    assert!(
        !r["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == plain),
        "{r}"
    );
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn a_second_server_started_first_leaves_the_agent_taken() {
    let bus = bus::Bus::start(&[]).await;
    let other = bus.connect().await;
    other
        .request_name(NAME)
        .await
        .expect("the first server owns the name");
    let k = agent(&bus).await;
    let s = state_where(&k, "taken", |s| {
        s["server"].as_str().is_some_and(|v| v.starts_with("taken"))
    })
    .await;
    assert_eq!(
        s["server"].as_str().unwrap(),
        format!("taken: {}", other.unique_name().unwrap())
    );
    assert_eq!(s["available"], false);
    let r = op(
        &k,
        "notifications",
        json!({"type": "notify", "summary": "x"}),
    )
    .await;
    assert_eq!(r["reason"], "unavailable", "{r}");
}
