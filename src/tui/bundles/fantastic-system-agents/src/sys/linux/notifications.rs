//! NOTIFICATIONS on the session bus: this process is the server of
//! `org.freedesktop.Notifications` (spec 1.2). `Notify` and
//! `CloseNotification` reach the ViewModel as events; the ViewModel's answers
//! (the id a `Notify` waits for, the `NotificationClosed` and `ActionInvoked`
//! signals) come back as calls. Spec: freedesktop.org notification-spec.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fantastic_system::notifications::{
    Call, Event, Hint, Notification, Ownership, CAPABILITIES, SERVER_NAME, SERVER_SPEC,
    SERVER_VENDOR, SERVER_VERSION,
};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use crate::sys::{NotificationsOs, OsError};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// The `Notify` calls that wait for an id, oldest first. A call is queued
/// before its event is sent, both under the lock, so the queue and the event
/// channel agree on the order; the ViewModel answers each `Notified` once.
type Waiting = Arc<AsyncMutex<VecDeque<oneshot::Sender<u32>>>>;

/// The served object.
struct Server {
    events: mpsc::Sender<Event>,
    waiting: Waiting,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The hints this agent reads, by their D-Bus types; the rest are dropped.
fn read_hints(hints: HashMap<String, OwnedValue>) -> BTreeMap<String, Hint> {
    let mut out = BTreeMap::new();
    for (key, value) in hints {
        let hint = match key.as_str() {
            "urgency" => u8::try_from(&value).ok().map(Hint::U8),
            "category" | "desktop-entry" => <&str>::try_from(&value)
                .ok()
                .map(|s| Hint::Str(s.to_string())),
            "resident" => bool::try_from(&value).ok().map(Hint::Bool),
            _ => None,
        };
        if let Some(h) = hint {
            out.insert(key, h);
        }
    }
    out
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    async fn get_capabilities(&self) -> Vec<String> {
        CAPABILITIES.iter().map(|c| c.to_string()).collect()
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let (tx, rx) = oneshot::channel();
        {
            let mut queue = self.waiting.lock().await;
            queue.push_back(tx);
            let ev = Event::Notified(Notification {
                app: app_name,
                replaces_id,
                icon: app_icon,
                summary,
                body,
                actions,
                hints: read_hints(hints),
                expire_timeout,
                now_ms: now_ms(),
            });
            if self.events.send(ev).await.is_err() {
                queue.pop_back();
                return 0;
            }
        }
        rx.await.unwrap_or(0)
    }

    async fn close_notification(&self, id: u32) {
        let _ = self.events.send(Event::CloseRequested { id }).await;
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            SERVER_NAME.into(),
            SERVER_VENDOR.into(),
            SERVER_VERSION.into(),
            SERVER_SPEC.into(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::Result<()>;
}

/// The notification server on a given session-bus connection.
pub struct LinuxNotifications {
    conn: zbus::Connection,
    events: mpsc::Sender<Event>,
    rx: Mutex<Option<mpsc::Receiver<Event>>>,
    waiting: Waiting,
}

impl LinuxNotifications {
    pub fn new(session: zbus::Connection) -> Self {
        let (events, rx) = mpsc::channel(64);
        LinuxNotifications {
            conn: session,
            events,
            rx: Mutex::new(Some(rx)),
            waiting: Arc::new(AsyncMutex::new(VecDeque::new())),
        }
    }
}

fn failed(e: zbus::Error) -> OsError {
    OsError::Failed(e.to_string())
}

#[async_trait::async_trait]
impl NotificationsOs for LinuxNotifications {
    async fn request(&self) -> Result<Ownership, OsError> {
        let server = Server {
            events: self.events.clone(),
            waiting: Arc::clone(&self.waiting),
        };
        // the object first: a call that arrives with the name must find it
        self.conn
            .object_server()
            .at(PATH, server)
            .await
            .map_err(failed)?;
        // zbus answers a name held by another daemon (with DoNotQueue) as `NameTaken`
        let taken = match self
            .conn
            .request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into())
            .await
        {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => false,
            Ok(RequestNameReply::Exists | RequestNameReply::InQueue)
            | Err(zbus::Error::NameTaken) => true,
            Err(e) => return Err(failed(e)),
        };
        if !taken {
            return Ok(Ownership::Owned);
        }
        let _ = self.conn.object_server().remove::<Server, _>(PATH).await;
        let dbus = zbus::fdo::DBusProxy::new(&self.conn)
            .await
            .map_err(|e| failed(e.into()))?;
        let name = zbus::names::BusName::try_from(NAME).map_err(|e| failed(e.into()))?;
        let by = dbus
            .get_name_owner(name)
            .await
            .map(|o| o.to_string())
            .unwrap_or_else(|_| "another server".to_string());
        Ok(Ownership::Taken { by })
    }

    async fn call(&self, c: Call) -> Result<(), OsError> {
        match c {
            Call::Request => self.request().await.map(|_| ()),
            Call::Reply { id } => {
                if let Some(tx) = self.waiting.lock().await.pop_front() {
                    let _ = tx.send(id);
                }
                Ok(())
            }
            Call::Closed { id, reason } => {
                let emitter = SignalEmitter::new(&self.conn, PATH).map_err(failed)?;
                Server::notification_closed(&emitter, id, reason)
                    .await
                    .map_err(failed)
            }
            Call::ActionInvoked { id, key } => {
                let emitter = SignalEmitter::new(&self.conn, PATH).map_err(failed)?;
                Server::action_invoked(&emitter, id, key)
                    .await
                    .map_err(failed)
            }
        }
    }

    fn changes(&self) -> mpsc::Receiver<Event> {
        self.rx
            .lock()
            .expect("rx poisoned")
            .take()
            .unwrap_or_else(|| mpsc::channel(1).1)
    }
}
