//! THE NOTIFICATIONS AGENT: the session's notification server
//! (`org.freedesktop.Notifications`). The sys layer serves the D-Bus interface
//! and hands every `Notify` and `CloseNotification` over as an event; this
//! module owns the ids, `replaces_id`, the expiry, the cap, the hints and
//! decides which signals go out.

use std::collections::BTreeMap;

use serde::Serialize;

/// The default lifetime of a notification that asks for `-1`, in milliseconds.
pub const DEFAULT_EXPIRE_MS: u64 = 8_000;
/// How many notifications are kept; the oldest is dropped beyond it.
pub const MAX_ITEMS: usize = 50;
/// `GetCapabilities`.
pub const CAPABILITIES: [&str; 3] = ["body", "actions", "persistence"];
/// `GetServerInformation`: name, vendor, spec version (the version is the crate's).
pub const SERVER_NAME: &str = "fantastic";
pub const SERVER_VENDOR: &str = "aisixteen";
pub const SERVER_SPEC: &str = "1.2";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `NotificationClosed` reasons.
pub const EXPIRED: u32 = 1;
pub const DISMISSED: u32 = 2;
pub const CLOSED_BY_CALL: u32 = 3;
pub const UNDEFINED: u32 = 4;

/// The app name of a notification an agent posts.
pub const APP: &str = "fantastic";

/// One hint's value, when it has a type this module reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hint {
    U8(u8),
    Str(String),
    Bool(bool),
}

/// A `Notify` call, as plain data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub app: String,
    pub replaces_id: u32,
    pub icon: String,
    pub summary: String,
    pub body: String,
    /// `[key, label, key, label, …]`, as on the wire.
    pub actions: Vec<String>,
    pub hints: BTreeMap<String, Hint>,
    pub expire_timeout: i32,
    pub now_ms: u64,
}

/// Whether this process owns the bus name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ownership {
    Owned,
    /// Another daemon holds it (its unique name).
    Taken {
        by: String,
    },
}

/// What a verb brings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub id: Option<u32>,
    pub action: Option<String>,
    pub summary: Option<String>,
    pub body: Option<String>,
    pub urgency: Option<String>,
    pub on: Option<bool>,
    pub now_ms: u64,
}

/// A bus call the port makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// Own the name, without queueing and without replacing another daemon.
    Request,
    /// The `NotificationClosed` signal.
    Closed { id: u32, reason: u32 },
    /// The `ActionInvoked` signal.
    ActionInvoked { id: u32, key: String },
    /// The answer to the `Notify` call that is waiting.
    Reply { id: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The first event of a boot: ask for the name.
    Start,
    /// The answer to [`Call::Request`].
    Ownership(Ownership),
    /// A client's `Notify`.
    Notified(Notification),
    /// A client's `CloseNotification`.
    CloseRequested { id: u32 },
    /// A verb: `notify`, `dismiss`, `dismiss_all`, `invoke` or `set_dnd`.
    Intent { verb: String, args: Args },
    /// The session bus is not there (or the request failed).
    Unavailable { why: String },
    /// Time, for the expiry.
    Tick { now_ms: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Call(Call),
    /// The state forbids the verb: the reply is this refusal.
    Refuse {
        verb: String,
        reason: String,
        message: String,
    },
}

#[derive(Clone, Debug, Default)]
pub struct Config {}

/// One notification, as the view draws it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Item {
    pub id: u32,
    pub app: String,
    pub summary: String,
    pub body: String,
    pub urgency: String,
    pub actions: Vec<(String, String)>,
    pub at_ms: u64,
    /// When it expires; `null` never.
    pub expires_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_entry: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub resident: bool,
    /// It arrived while do-not-disturb was on.
    #[serde(skip_serializing_if = "is_false")]
    pub quiet: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// What the view draws; its JSON is the agent's `state` reply.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct State {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// `owned`, `taken: <unique name>`, or `none` before the bus answers.
    pub server: String,
    /// Oldest first.
    pub items: Vec<Item>,
    pub do_not_disturb: bool,
}

/// `urgency` (0, 1, 2) as a name.
pub fn urgency_name(code: u8) -> &'static str {
    match code {
        0 => "low",
        2 => "critical",
        _ => "normal",
    }
}

fn urgency_code(name: &str) -> Option<u8> {
    match name {
        "low" => Some(0),
        "normal" => Some(1),
        "critical" => Some(2),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Server {
    None,
    Owned,
    Taken(String),
}

pub struct NotificationsVm {
    server: Server,
    why: Option<String>,
    items: Vec<Item>,
    next_id: u32,
    dnd: bool,
}

impl NotificationsVm {
    pub fn new(_cfg: Config) -> Self {
        NotificationsVm {
            server: Server::None,
            why: Some("not read yet".into()),
            items: vec![],
            next_id: 1,
            dnd: false,
        }
    }

    pub fn state(&self) -> State {
        State {
            available: self.server == Server::Owned,
            why: self.why.clone(),
            server: match &self.server {
                Server::None => "none".into(),
                Server::Owned => "owned".into(),
                Server::Taken(by) => format!("taken: {by}"),
            },
            items: self.items.clone(),
            do_not_disturb: self.dnd,
        }
    }

    pub fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        match ev {
            Event::Start => vec![Effect::Call(Call::Request)],
            Event::Ownership(Ownership::Owned) => {
                self.server = Server::Owned;
                self.why = None;
                vec![]
            }
            Event::Ownership(Ownership::Taken { by }) => {
                self.why = Some(format!("org.freedesktop.Notifications is held by {by}"));
                self.server = Server::Taken(by);
                self.items.clear();
                vec![]
            }
            Event::Unavailable { why } => {
                self.server = Server::None;
                self.why = Some(why);
                self.items.clear();
                vec![]
            }
            Event::Notified(n) => self.notified(n),
            Event::CloseRequested { id } => {
                if self.remove(id) {
                    vec![closed(id, CLOSED_BY_CALL)]
                } else {
                    vec![]
                }
            }
            Event::Tick { now_ms } => self.expire(now_ms),
            Event::Intent { verb, args } => self.intent(&verb, &args),
        }
    }

    /// The next free id: from 1, never 0, none that is in use.
    fn fresh_id(&mut self) -> u32 {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.checked_add(1).unwrap_or(1);
            if id != 0 && !self.items.iter().any(|i| i.id == id) {
                return id;
            }
        }
    }

    fn remove(&mut self, id: u32) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.id != id);
        self.items.len() != before
    }

    fn notified(&mut self, n: Notification) -> Vec<Effect> {
        let urgency = match n.hints.get("urgency") {
            Some(Hint::U8(u)) => *u,
            _ => 1,
        };
        let text = |k: &str| match n.hints.get(k) {
            Some(Hint::Str(s)) => Some(s.clone()),
            _ => None,
        };
        // -1: the server's default; 0: never; critical: never
        let lifetime = match n.expire_timeout {
            0 => None,
            t if t < 0 => Some(DEFAULT_EXPIRE_MS),
            t => Some(t as u64),
        }
        .filter(|_| urgency != 2);
        let mut item = Item {
            id: 0,
            app: n.app,
            summary: n.summary,
            body: n.body,
            urgency: urgency_name(urgency).into(),
            actions: n
                .actions
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| (c[0].clone(), c[1].clone()))
                .collect(),
            at_ms: n.now_ms,
            expires_ms: lifetime.map(|l| n.now_ms.saturating_add(l)),
            category: text("category"),
            desktop_entry: text("desktop-entry"),
            resident: matches!(n.hints.get("resident"), Some(Hint::Bool(true))),
            quiet: self.dnd,
        };
        let mut effects = vec![];
        match self
            .items
            .iter()
            .position(|i| n.replaces_id != 0 && i.id == n.replaces_id)
        {
            Some(at) => {
                item.id = n.replaces_id;
                self.items[at] = item;
                effects.push(Effect::Call(Call::Reply { id: n.replaces_id }));
            }
            None => {
                item.id = self.fresh_id();
                effects.push(Effect::Call(Call::Reply { id: item.id }));
                self.items.push(item);
                while self.items.len() > MAX_ITEMS {
                    let gone = self.items.remove(0);
                    effects.push(closed(gone.id, UNDEFINED));
                }
            }
        }
        effects
    }

    fn expire(&mut self, now_ms: u64) -> Vec<Effect> {
        let due: Vec<u32> = self
            .items
            .iter()
            .filter(|i| i.expires_ms.is_some_and(|t| now_ms >= t))
            .map(|i| i.id)
            .collect();
        self.items.retain(|i| !due.contains(&i.id));
        due.into_iter().map(|id| closed(id, EXPIRED)).collect()
    }

    fn intent(&mut self, verb: &str, a: &Args) -> Vec<Effect> {
        let refuse = |reason: &str, message: String| {
            vec![Effect::Refuse {
                verb: verb.into(),
                reason: reason.into(),
                message,
            }]
        };
        if !matches!(
            verb,
            "notify" | "dismiss" | "dismiss_all" | "invoke" | "set_dnd"
        ) {
            return refuse(
                "unsupported",
                format!("notifications: unknown verb `{verb}`"),
            );
        }
        if self.server != Server::Owned {
            let why = self.why.clone().unwrap_or_default();
            return refuse("unavailable", format!("{verb}: {why}"));
        }
        match verb {
            "notify" => {
                let Some(summary) = a.summary.clone().filter(|s| !s.is_empty()) else {
                    return refuse("bad_arg", "notify: needs a `summary`".into());
                };
                let urgency = match a.urgency.as_deref() {
                    None => 1,
                    Some(name) => match urgency_code(name) {
                        Some(u) => u,
                        None => {
                            return refuse(
                                "bad_arg",
                                "notify: `urgency` is low, normal or critical".into(),
                            )
                        }
                    },
                };
                let mut effects = self.notified(Notification {
                    app: APP.into(),
                    replaces_id: 0,
                    icon: String::new(),
                    summary,
                    body: a.body.clone().unwrap_or_default(),
                    actions: vec![],
                    hints: BTreeMap::from([("urgency".to_string(), Hint::U8(urgency))]),
                    expire_timeout: -1,
                    now_ms: a.now_ms,
                });
                // nobody waits for this one's id: the reply is the state
                effects.retain(|e| !matches!(e, Effect::Call(Call::Reply { .. })));
                effects
            }
            "dismiss" => {
                let Some(id) = a.id else {
                    return refuse(
                        "bad_arg",
                        "dismiss: needs the `id` of a notification".into(),
                    );
                };
                if self.remove(id) {
                    vec![closed(id, DISMISSED)]
                } else {
                    refuse("not_found", format!("dismiss: no notification {id}"))
                }
            }
            "dismiss_all" => std::mem::take(&mut self.items)
                .into_iter()
                .map(|i| closed(i.id, DISMISSED))
                .collect(),
            "invoke" => {
                let (Some(id), Some(action)) = (a.id, a.action.clone()) else {
                    return refuse(
                        "bad_arg",
                        "invoke: needs the `id` and the `action` key".into(),
                    );
                };
                let Some(item) = self.items.iter().find(|i| i.id == id) else {
                    return refuse("not_found", format!("invoke: no notification {id}"));
                };
                if !item.actions.iter().any(|(k, _)| *k == action) {
                    return refuse(
                        "bad_arg",
                        format!("invoke: notification {id} has no action `{action}`"),
                    );
                }
                let resident = item.resident;
                let mut effects = vec![Effect::Call(Call::ActionInvoked { id, key: action })];
                if !resident {
                    self.remove(id);
                    effects.push(closed(id, DISMISSED));
                }
                effects
            }
            _ => match a.on {
                Some(on) => {
                    self.dnd = on;
                    vec![]
                }
                None => refuse("bad_arg", "set_dnd: needs `on`, true or false".into()),
            },
        }
    }
}

fn closed(id: u32, reason: u32) -> Effect {
    Effect::Call(Call::Closed { id, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned() -> NotificationsVm {
        let mut vm = NotificationsVm::new(Config::default());
        assert_eq!(vm.reduce(Event::Start), vec![Effect::Call(Call::Request)]);
        vm.reduce(Event::Ownership(Ownership::Owned));
        vm
    }

    fn note(summary: &str) -> Notification {
        Notification {
            app: "notify-send".into(),
            replaces_id: 0,
            icon: String::new(),
            summary: summary.into(),
            body: "b".into(),
            actions: vec![],
            hints: BTreeMap::new(),
            expire_timeout: -1,
            now_ms: 1_000,
        }
    }

    fn notify(vm: &mut NotificationsVm, n: Notification) -> u32 {
        match vm.reduce(Event::Notified(n)).first() {
            Some(Effect::Call(Call::Reply { id })) => *id,
            other => panic!("no reply: {other:?}"),
        }
    }

    fn intent(vm: &mut NotificationsVm, verb: &str, args: Args) -> Vec<Effect> {
        vm.reduce(Event::Intent {
            verb: verb.into(),
            args,
        })
    }

    fn id_arg(id: u32) -> Args {
        Args {
            id: Some(id),
            ..Args::default()
        }
    }

    fn reason(effects: &[Effect]) -> &str {
        match &effects[0] {
            Effect::Refuse { reason, .. } => reason,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    fn ids(vm: &NotificationsVm) -> Vec<u32> {
        vm.state().items.iter().map(|i| i.id).collect()
    }

    #[test]
    fn notify_assigns_ids_from_one_and_never_zero() {
        let mut vm = owned();
        assert_eq!(notify(&mut vm, note("a")), 1);
        assert_eq!(notify(&mut vm, note("b")), 2);
        // the counter wraps past u32::MAX to 1, never to 0
        vm.next_id = u32::MAX;
        assert_eq!(notify(&mut vm, note("c")), u32::MAX);
        let id = notify(&mut vm, note("d"));
        assert_ne!(id, 0);
        assert_eq!(
            id, 3,
            "the counter wrapped to 1 and 2: both in use, so the next free one"
        );
    }

    #[test]
    fn replaces_id_replaces_the_item_in_place() {
        let mut vm = owned();
        let a = notify(&mut vm, note("a"));
        let b = notify(&mut vm, note("b"));
        let mut again = note("a2");
        again.replaces_id = a;
        again.now_ms = 5_000;
        assert_eq!(notify(&mut vm, again), a, "the same id comes back");
        let s = vm.state();
        assert_eq!(ids(&vm), vec![a, b], "same position");
        assert_eq!(s.items[0].summary, "a2");
        assert_eq!(s.items[0].at_ms, 5_000);
        // an id that no longer exists is a new notification
        let mut gone = note("g");
        gone.replaces_id = 99;
        let id = notify(&mut vm, gone);
        assert_ne!(id, 99);
        assert_eq!(vm.state().items.len(), 3);
    }

    #[test]
    fn expiry_by_tick_uses_8000ms_default_zero_never_and_critical_never() {
        let mut vm = owned();
        let default = notify(&mut vm, note("default")); // -1 at 1000: 9000
        let mut n = note("short");
        n.expire_timeout = 2_000; // 3000
        let short = notify(&mut vm, n);
        let mut n = note("never");
        n.expire_timeout = 0;
        let never = notify(&mut vm, n);
        let mut n = note("critical");
        n.hints.insert("urgency".into(), Hint::U8(2));
        let critical = notify(&mut vm, n);
        let s = vm.state();
        let by = |id| s.items.iter().find(|i| i.id == id).unwrap().expires_ms;
        assert_eq!(
            (by(default), by(short), by(never), by(critical)),
            (Some(9_000), Some(3_000), None, None)
        );
        assert!(vm.reduce(Event::Tick { now_ms: 2_999 }).is_empty());
        assert_eq!(
            vm.reduce(Event::Tick { now_ms: 3_000 }),
            vec![closed(short, EXPIRED)]
        );
        assert_eq!(
            vm.reduce(Event::Tick { now_ms: 9_000 }),
            vec![closed(default, EXPIRED)]
        );
        assert!(vm.reduce(Event::Tick { now_ms: 10_000_000 }).is_empty());
        assert_eq!(ids(&vm), vec![never, critical]);
    }

    #[test]
    fn items_are_capped_at_50_oldest_dropped() {
        let mut vm = owned();
        for n in 0..MAX_ITEMS {
            notify(&mut vm, note(&format!("n{n}")));
        }
        assert_eq!(vm.state().items.len(), 50);
        let effects = vm.reduce(Event::Notified(note("one more")));
        assert_eq!(effects[0], Effect::Call(Call::Reply { id: 51 }));
        assert_eq!(
            effects[1],
            closed(1, UNDEFINED),
            "the oldest is closed with reason 4"
        );
        let s = vm.state();
        assert_eq!(s.items.len(), 50);
        assert_eq!((s.items[0].id, s.items[49].id), (2, 51));
    }

    #[test]
    fn close_notification_emits_closed_with_reason_3() {
        let mut vm = owned();
        let id = notify(&mut vm, note("a"));
        assert_eq!(
            vm.reduce(Event::CloseRequested { id }),
            vec![closed(id, CLOSED_BY_CALL)]
        );
        assert!(vm.state().items.is_empty());
        assert!(
            vm.reduce(Event::CloseRequested { id }).is_empty(),
            "closing it twice signals once"
        );
    }

    #[test]
    fn dismiss_uses_reason_2_and_expiry_reason_1() {
        let mut vm = owned();
        let a = notify(&mut vm, note("a"));
        let b = notify(&mut vm, note("b"));
        let c = notify(&mut vm, note("c"));
        assert_eq!(
            intent(&mut vm, "dismiss", id_arg(a)),
            vec![closed(a, DISMISSED)]
        );
        assert_eq!(reason(&intent(&mut vm, "dismiss", id_arg(a))), "not_found");
        assert_eq!(
            reason(&intent(&mut vm, "dismiss", Args::default())),
            "bad_arg"
        );
        assert_eq!(
            intent(&mut vm, "dismiss_all", Args::default()),
            vec![closed(b, DISMISSED), closed(c, DISMISSED)]
        );
        assert!(vm.state().items.is_empty());
        let d = notify(&mut vm, note("d"));
        assert_eq!(
            vm.reduce(Event::Tick { now_ms: 1_000_000 }),
            vec![closed(d, EXPIRED)]
        );
    }

    #[test]
    fn invoke_emits_action_invoked_then_closes_unless_resident() {
        let mut vm = owned();
        let mut plain = note("plain");
        plain.actions = vec!["default".into(), "Open".into()];
        let plain = notify(&mut vm, plain);
        let mut resident = note("resident");
        resident.actions = vec!["default".into(), "Open".into()];
        resident.hints.insert("resident".into(), Hint::Bool(true));
        let resident = notify(&mut vm, resident);
        let act = |id| Args {
            id: Some(id),
            action: Some("default".into()),
            ..Args::default()
        };
        assert_eq!(
            intent(&mut vm, "invoke", act(plain)),
            vec![
                Effect::Call(Call::ActionInvoked {
                    id: plain,
                    key: "default".into()
                }),
                closed(plain, DISMISSED)
            ]
        );
        assert_eq!(
            intent(&mut vm, "invoke", act(resident)),
            vec![Effect::Call(Call::ActionInvoked {
                id: resident,
                key: "default".into()
            })]
        );
        assert_eq!(ids(&vm), vec![resident], "a resident item stays");
        assert_eq!(reason(&intent(&mut vm, "invoke", act(plain))), "not_found");
        let wrong = Args {
            action: Some("nope".into()),
            ..id_arg(resident)
        };
        assert_eq!(reason(&intent(&mut vm, "invoke", wrong)), "bad_arg");
        assert_eq!(
            reason(&intent(&mut vm, "invoke", id_arg(resident))),
            "bad_arg"
        );
    }

    #[test]
    fn dnd_marks_items_quiet_but_keeps_them() {
        let mut vm = owned();
        notify(&mut vm, note("before"));
        let on = Args {
            on: Some(true),
            ..Args::default()
        };
        assert!(intent(&mut vm, "set_dnd", on).is_empty());
        notify(&mut vm, note("during"));
        let s = vm.state();
        assert!(s.do_not_disturb);
        assert_eq!(s.items.len(), 2, "items still arrive");
        assert_eq!((s.items[0].quiet, s.items[1].quiet), (false, true));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["items"][1]["quiet"], true);
        assert!(v["items"][0].get("quiet").is_none());
        let off = Args {
            on: Some(false),
            ..Args::default()
        };
        intent(&mut vm, "set_dnd", off);
        assert!(!vm.state().do_not_disturb);
        assert_eq!(
            reason(&intent(&mut vm, "set_dnd", Args::default())),
            "bad_arg"
        );
    }

    #[test]
    fn taken_server_state_is_taken_with_unique_name_and_notify_verb_refused() {
        let mut vm = NotificationsVm::new(Config::default());
        assert_eq!(vm.state().server, "none");
        vm.reduce(Event::Ownership(Ownership::Taken { by: ":1.42".into() }));
        let s = vm.state();
        assert_eq!(s.server, "taken: :1.42");
        assert!(!s.available);
        assert!(s.why.unwrap().contains(":1.42"));
        let notify_args = Args {
            summary: Some("hi".into()),
            ..Args::default()
        };
        let r = intent(&mut vm, "notify", notify_args);
        assert_eq!(reason(&r), "unavailable");
        for verb in ["dismiss", "dismiss_all", "invoke", "set_dnd"] {
            assert_eq!(
                reason(&intent(&mut vm, verb, id_arg(1))),
                "unavailable",
                "{verb}"
            );
        }
        // owning it afterwards is available again
        vm.reduce(Event::Ownership(Ownership::Owned));
        assert!(vm.state().available && vm.state().server == "owned");
        vm.reduce(Event::Unavailable {
            why: "the session bus is not reachable".into(),
        });
        let s = vm.state();
        assert_eq!((s.available, s.server.as_str()), (false, "none"));
    }

    #[test]
    fn hints_read_urgency_category_desktop_entry_resident_and_ignore_the_rest() {
        let mut vm = owned();
        let mut n = note("hints");
        n.hints = BTreeMap::from([
            ("urgency".to_string(), Hint::U8(2)),
            ("category".to_string(), Hint::Str("email.arrived".into())),
            (
                "desktop-entry".to_string(),
                Hint::Str("org.example.Mail".into()),
            ),
            ("resident".to_string(), Hint::Bool(true)),
            ("x".to_string(), Hint::U8(7)),
            ("sound-name".to_string(), Hint::Str("bell".into())),
        ]);
        n.actions = vec!["default".into(), "Open".into(), "dangling".into()];
        notify(&mut vm, n);
        let i = vm.state().items.remove(0);
        assert_eq!(i.urgency, "critical");
        assert_eq!(i.category.as_deref(), Some("email.arrived"));
        assert_eq!(i.desktop_entry.as_deref(), Some("org.example.Mail"));
        assert!(i.resident);
        assert_eq!(i.actions, vec![("default".to_string(), "Open".to_string())]);
        // hints of the wrong type are ignored
        let mut n = note("wrong types");
        n.hints = BTreeMap::from([
            ("urgency".to_string(), Hint::Str("critical".into())),
            ("category".to_string(), Hint::U8(1)),
            ("resident".to_string(), Hint::Str("true".into())),
        ]);
        notify(&mut vm, n);
        let i = vm.state().items.remove(1);
        assert_eq!(i.urgency, "normal");
        assert!(i.category.is_none() && !i.resident);
        assert_eq!(urgency_name(0), "low");
    }

    #[test]
    fn notify_verb_posts_as_fantastic_without_waiting_for_a_reply() {
        let mut vm = owned();
        let args = Args {
            summary: Some("build done".into()),
            body: Some("all green".into()),
            urgency: Some("low".into()),
            now_ms: 4_000,
            ..Args::default()
        };
        assert!(intent(&mut vm, "notify", args).is_empty());
        let i = vm.state().items.remove(0);
        assert_eq!(
            (
                i.app.as_str(),
                i.summary.as_str(),
                i.body.as_str(),
                i.urgency.as_str()
            ),
            ("fantastic", "build done", "all green", "low")
        );
        assert_eq!((i.at_ms, i.expires_ms), (4_000, Some(12_000)));
        let none = Args::default();
        assert_eq!(reason(&intent(&mut vm, "notify", none)), "bad_arg");
        let bad = Args {
            summary: Some("x".into()),
            urgency: Some("loud".into()),
            ..Args::default()
        };
        assert_eq!(reason(&intent(&mut vm, "notify", bad)), "bad_arg");
        assert_eq!(
            reason(&intent(&mut vm, "nope", Args::default())),
            "unsupported"
        );
    }

    #[test]
    fn the_server_answers_are_the_documented_ones() {
        assert_eq!(CAPABILITIES, ["body", "actions", "persistence"]);
        assert_eq!(
            (SERVER_NAME, SERVER_VENDOR, SERVER_SPEC),
            ("fantastic", "aisixteen", "1.2")
        );
        assert!(!SERVER_VERSION.is_empty());
    }
}
