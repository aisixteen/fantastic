//! THE POWER AGENT: battery and lid over UPower, power off / reboot / suspend /
//! hibernate over logind. The sys layer flattens both services' D-Bus property
//! maps into a [`Read`]; this module decides everything else.

use serde::Serialize;

/// How often the `Can*` answers are read again (polkit's answer follows the
/// session's state), in milliseconds.
pub const REREAD_MS: u64 = 30_000;

/// logind's `Can*` answers: `yes`, `no`, `challenge`, `na`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Can {
    pub power_off: String,
    pub reboot: String,
    pub suspend: String,
    pub hibernate: String,
}

impl Can {
    fn all(answer: &str) -> Self {
        Can {
            power_off: answer.into(),
            reboot: answer.into(),
            suspend: answer.into(),
            hibernate: answer.into(),
        }
    }
}

/// One read of both services, flattened.
#[derive(Clone, Debug, PartialEq)]
pub struct Read {
    pub on_battery: bool,
    pub lid_present: bool,
    pub lid_closed: bool,
    /// UPower's display device `IsPresent`.
    pub display_present: bool,
    /// `Type` (u): kept for the sys layer's fixtures; only `IsPresent` decides.
    pub display_type: u32,
    /// `State` (u), 0..=6.
    pub display_state: u32,
    pub percentage: f64,
    pub time_to_empty: i64,
    pub time_to_full: i64,
    /// `WarningLevel` (u), 0..=5.
    pub warning: u32,
    pub can: Can,
}

/// A logind power method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    PowerOff,
    Reboot,
    Suspend,
    Hibernate,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// An answer to [`Effect::Read`].
    Read(Read),
    /// Something changed on the bus: read again.
    Changed,
    /// logind's `PrepareForSleep(start)`.
    PrepareForSleep { start: bool },
    /// A verb: `power_off`, `reboot`, `suspend` or `hibernate`.
    Intent { verb: String },
    /// A service is not there (or a read failed).
    Unavailable { why: String },
    /// Time, for the periodic re-read.
    Tick { now_ms: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Read,
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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Battery {
    pub percent: f64,
    pub state: String,
    pub time_to_empty_s: i64,
    pub time_to_full_s: i64,
    pub warning: String,
}

/// What the view draws; its JSON is the agent's `state` reply.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct State {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub on_battery: bool,
    pub battery: Option<Battery>,
    pub lid_closed: Option<bool>,
    pub can: Can,
    pub sleeping: bool,
}

/// UPower `Device.State` (0..=6).
pub fn battery_state_name(code: u32) -> &'static str {
    match code {
        1 => "charging",
        2 => "discharging",
        3 => "empty",
        4 => "fully_charged",
        5 => "pending_charge",
        6 => "pending_discharge",
        _ => "unknown",
    }
}

/// UPower `Device.WarningLevel` (0..=5).
pub fn warning_name(code: u32) -> &'static str {
    match code {
        1 => "none",
        2 => "discharging",
        3 => "low",
        4 => "critical",
        5 => "action",
        _ => "unknown",
    }
}

pub struct PowerVm {
    state: State,
    /// When the `Can*` answers were last asked for, by a tick (None before the first).
    last_read_ms: Option<u64>,
}

impl PowerVm {
    pub fn new(_cfg: Config) -> Self {
        PowerVm {
            state: State {
                available: false,
                why: Some("not read yet".into()),
                on_battery: false,
                battery: None,
                lid_closed: None,
                can: Can::all("unavailable"),
                sleeping: false,
            },
            last_read_ms: None,
        }
    }

    pub fn state(&self) -> State {
        self.state.clone()
    }

    pub fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        match ev {
            Event::Read(r) => {
                self.apply(r);
                vec![]
            }
            Event::Changed => vec![Effect::Read],
            Event::PrepareForSleep { start } => {
                self.state.sleeping = start;
                if start {
                    vec![]
                } else {
                    vec![Effect::Read]
                }
            }
            Event::Unavailable { why } => {
                self.state.available = false;
                self.state.why = Some(why);
                vec![]
            }
            Event::Tick { now_ms } => {
                let last = *self.last_read_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(last) >= REREAD_MS {
                    self.last_read_ms = Some(now_ms);
                    vec![Effect::Read]
                } else {
                    vec![]
                }
            }
            Event::Intent { verb } => vec![self.intent(&verb)],
        }
    }

    fn apply(&mut self, r: Read) {
        let s = &mut self.state;
        s.available = true;
        s.why = None;
        s.on_battery = r.on_battery;
        s.battery = r.display_present.then(|| Battery {
            percent: r.percentage,
            state: battery_state_name(r.display_state).into(),
            time_to_empty_s: r.time_to_empty,
            time_to_full_s: r.time_to_full,
            warning: warning_name(r.warning).into(),
        });
        s.lid_closed = r.lid_present.then_some(r.lid_closed);
        s.can = r.can;
    }

    fn intent(&self, verb: &str) -> Effect {
        let refuse = |reason: &str, message: String| Effect::Refuse {
            verb: verb.into(),
            reason: reason.into(),
            message,
        };
        let (call, can) = match verb {
            "power_off" => (Call::PowerOff, &self.state.can.power_off),
            "reboot" => (Call::Reboot, &self.state.can.reboot),
            "suspend" => (Call::Suspend, &self.state.can.suspend),
            "hibernate" => (Call::Hibernate, &self.state.can.hibernate),
            other => return refuse("unsupported", format!("power: unknown verb `{other}`")),
        };
        if !self.state.available {
            let why = self.state.why.clone().unwrap_or_default();
            return refuse("unavailable", format!("{verb}: {why}"));
        }
        match can.as_str() {
            "yes" => Effect::Call(call),
            "challenge" => refuse(
                "denied",
                format!("{verb}: needs authentication — is the polkit rule installed?"),
            ),
            "no" | "na" => refuse(
                "unsupported",
                format!("{verb}: this machine cannot ({can})"),
            ),
            _ => refuse("unavailable", format!("{verb}: not known yet")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read() -> Read {
        Read {
            on_battery: false,
            lid_present: false,
            lid_closed: false,
            display_present: true,
            display_type: 2,
            display_state: 2,
            percentage: 81.0,
            time_to_empty: 9120,
            time_to_full: 0,
            warning: 1,
            can: Can {
                power_off: "yes".into(),
                reboot: "yes".into(),
                suspend: "yes".into(),
                hibernate: "na".into(),
            },
        }
    }

    fn vm_with(r: Read) -> PowerVm {
        let mut vm = PowerVm::new(Config::default());
        vm.reduce(Event::Read(r));
        vm
    }

    #[test]
    fn upower_state_and_warning_codes_map_to_names() {
        let states = [
            "unknown",
            "charging",
            "discharging",
            "empty",
            "fully_charged",
            "pending_charge",
            "pending_discharge",
        ];
        let warnings = [
            "unknown",
            "none",
            "discharging",
            "low",
            "critical",
            "action",
        ];
        for (code, name) in states.iter().enumerate() {
            let mut r = read();
            r.display_state = code as u32;
            let b = vm_with(r).state().battery.unwrap();
            assert_eq!(b.state, *name, "State {code}");
        }
        for (code, name) in warnings.iter().enumerate() {
            let mut r = read();
            r.warning = code as u32;
            let b = vm_with(r).state().battery.unwrap();
            assert_eq!(b.warning, *name, "WarningLevel {code}");
        }
        let mut r = read();
        (r.display_state, r.warning) = (99, 99);
        let b = vm_with(r).state().battery.unwrap();
        assert_eq!(
            (b.state.as_str(), b.warning.as_str()),
            ("unknown", "unknown")
        );
    }

    #[test]
    fn battery_is_null_without_a_present_display_device_and_lid_null_without_lid() {
        let mut r = read();
        r.display_present = false;
        r.lid_closed = true; // a stale property on a machine with no lid
        let s = vm_with(r.clone()).state();
        assert!(s.battery.is_none() && s.lid_closed.is_none());
        let j = serde_json::to_value(&s).unwrap();
        assert!(j["battery"].is_null() && j["lid_closed"].is_null(), "{j}");
        r.lid_present = true;
        assert_eq!(vm_with(r).state().lid_closed, Some(true));
    }

    #[test]
    fn the_state_serialises_as_the_documented_json() {
        let j = serde_json::to_value(vm_with(read()).state()).unwrap();
        assert_eq!(
            j,
            serde_json::json!({
                "available": true, "on_battery": false,
                "battery": {"percent": 81.0, "state": "discharging", "time_to_empty_s": 9120,
                            "time_to_full_s": 0, "warning": "none"},
                "lid_closed": null,
                "can": {"power_off": "yes", "reboot": "yes", "suspend": "yes", "hibernate": "na"},
                "sleeping": false
            })
        );
        let mut vm = PowerVm::new(Config::default());
        vm.reduce(Event::Unavailable {
            why: "org.freedesktop.UPower is not on the system bus".into(),
        });
        let j = serde_json::to_value(vm.state()).unwrap();
        assert_eq!(j["available"], false);
        assert_eq!(j["why"], "org.freedesktop.UPower is not on the system bus");
    }

    fn intent(vm: &mut PowerVm, verb: &str) -> Effect {
        vm.reduce(Event::Intent { verb: verb.into() }).remove(0)
    }

    fn reason(e: &Effect) -> &str {
        match e {
            Effect::Refuse { reason, .. } => reason,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    #[test]
    fn intents_are_refused_by_can_answer() {
        let mut r = read();
        r.can = Can {
            power_off: "challenge".into(),
            reboot: "no".into(),
            suspend: "yes".into(),
            hibernate: "na".into(),
        };
        let mut vm = vm_with(r);
        let e = intent(&mut vm, "power_off");
        assert_eq!(reason(&e), "denied");
        assert!(
            matches!(&e, Effect::Refuse { message, .. } if message.contains("is the polkit rule installed?"))
        );
        assert_eq!(reason(&intent(&mut vm, "reboot")), "unsupported");
        assert_eq!(reason(&intent(&mut vm, "hibernate")), "unsupported");
        assert_eq!(intent(&mut vm, "suspend"), Effect::Call(Call::Suspend));
        // a service that is not there refuses everything as unavailable
        let mut gone = PowerVm::new(Config::default());
        gone.reduce(Event::Unavailable {
            why: "org.freedesktop.login1 is not on the system bus".into(),
        });
        assert_eq!(reason(&intent(&mut gone, "suspend")), "unavailable");
        // a Can answer the VM has not read yet is unavailable too
        let mut fresh = PowerVm::new(Config::default());
        assert_eq!(reason(&intent(&mut fresh, "reboot")), "unavailable");
    }

    #[test]
    fn changes_sleep_and_ticks_ask_for_a_read() {
        let mut vm = vm_with(read());
        assert_eq!(vm.reduce(Event::Changed), vec![Effect::Read]);
        assert!(vm.reduce(Event::PrepareForSleep { start: true }).is_empty());
        assert!(vm.state().sleeping);
        assert_eq!(
            vm.reduce(Event::PrepareForSleep { start: false }),
            vec![Effect::Read]
        );
        assert!(!vm.state().sleeping);
        // the first tick only sets the baseline; 30 s later the answers are read again
        assert!(vm.reduce(Event::Tick { now_ms: 1_000 }).is_empty());
        assert!(vm.reduce(Event::Tick { now_ms: 30_999 }).is_empty());
        assert_eq!(
            vm.reduce(Event::Tick { now_ms: 31_000 }),
            vec![Effect::Read]
        );
        assert!(vm.reduce(Event::Tick { now_ms: 40_000 }).is_empty());
        assert_eq!(
            vm.reduce(Event::Tick { now_ms: 61_000 }),
            vec![Effect::Read]
        );
    }
}
