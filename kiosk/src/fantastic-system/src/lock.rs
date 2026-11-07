//! THE LOCK AGENT: the kiosk's lock screen. The screen is drawn by the Home
//! page, which obeys `state.locked`; this module owns everything behind it:
//! when the screen locks (a verb, logind's `Lock`, the idle clock, sleep), what
//! unlocks it (a PIN of 4 to 12 digits), how a run of wrong PINs is slowed
//! down, and which logind calls mirror the state. The PIN is hashed and kept
//! by the sys layer (`CheckPin`, `Persist`). It is a kiosk UI lock, not a
//! security boundary: code that already runs as the user is not stopped by it.

use std::fmt;

use serde::Serialize;

/// How many wrong PINs in a row before the waiting starts.
pub const FREE_TRIES: u32 = 5;
/// The first wait, and the longest one, in milliseconds.
pub const FIRST_WAIT_MS: u64 = 30_000;
pub const MAX_WAIT_MS: u64 = 15 * 60_000;
/// `activity` is taken at most this often, in milliseconds.
pub const ACTIVITY_EVERY_MS: u64 = 10_000;
/// The longest auto-lock setting, in minutes (a day).
pub const MAX_AUTO_LOCK_MIN: u32 = 1_440;
/// The auto-lock setting of a machine that never set one.
pub const DEFAULT_AUTO_LOCK_MIN: u32 = 10;

/// A PIN that never prints: no `Serialize`, a redacted `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Pin(String);

impl Pin {
    pub fn new(s: impl Into<String>) -> Self {
        Pin(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pin(<redacted>)")
    }
}

/// A PIN is 4 to 12 digits.
pub fn pin_fits(pin: &str) -> bool {
    (4..=12).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

/// What the sys layer read: the persisted part of the state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    pub has_pin: bool,
    pub auto_lock_min: u32,
}

/// What a verb brings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub pin: Option<Pin>,
    pub old: Option<Pin>,
    pub new: Option<Pin>,
    pub minutes: Option<u32>,
    /// A `minutes` that is no whole number.
    pub bad_minutes: bool,
    /// The clock at the verb, in milliseconds: the wait after wrong PINs counts from it.
    pub now_ms: u64,
}

/// What `Persist` does to the stored PIN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinChange {
    Keep,
    Set(Pin),
    Clear,
}

/// A call the port makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// Mirror `locked` into the display session's `LockedHint`.
    SetLockedHint { locked: bool },
    /// Take (`true`) or release (`false`) the sleep inhibitor.
    HoldInhibitor { hold: bool },
    /// Write `<home>/lock.yaml`.
    Persist { pin: PinChange, auto_lock_min: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The first event of a boot: read the stored settings, take the inhibitor.
    Start,
    /// An answer to [`Effect::Read`].
    Read(Read),
    /// A verb: `lock`, `unlock`, `set_pin`, `clear_pin` or `set_auto_lock`.
    Intent { verb: String, args: Args },
    /// The answer to [`Effect::CheckPin`].
    PinChecked { ok: bool },
    /// The check itself failed (the stored hash is unreadable).
    PinError { why: String },
    /// logind's `Lock` signal on the display session.
    LogindLock,
    /// logind's `Unlock` signal on the display session.
    LogindUnlock,
    /// logind's `PrepareForSleep`.
    PrepareForSleep { start: bool },
    /// Time, for the idle clock and the wait after wrong PINs.
    Tick { now_ms: u64 },
    /// The verb `activity`: input on the Home page.
    Activity { now_ms: u64 },
    /// logind or the settings are not there (or a read failed).
    Unavailable { why: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Read,
    /// Hash `pin` and compare it with the stored hash.
    CheckPin {
        pin: Pin,
    },
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

/// What the view draws; its JSON is the agent's `state` reply.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct State {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub locked: bool,
    pub has_pin: bool,
    pub auto_lock_min: u32,
    pub failures: u32,
    pub retry_at_ms: Option<u64>,
}

/// What a PIN check is for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    Unlock { verb: String },
    SetPin { new: Pin },
    ClearPin,
}

pub struct LockVm {
    available: bool,
    why: Option<String>,
    locked: bool,
    has_pin: bool,
    auto_lock_min: u32,
    failures: u32,
    retry_at_ms: Option<u64>,
    pending: Option<Pending>,
    now_ms: u64,
    last_activity_ms: Option<u64>,
    held: bool,
}

/// The wait after the `failures`-th wrong PIN in a row (5 or more).
pub fn wait_ms(failures: u32) -> u64 {
    let doublings = failures.saturating_sub(FREE_TRIES).min(16);
    (FIRST_WAIT_MS << doublings).min(MAX_WAIT_MS)
}

impl LockVm {
    pub fn new(_cfg: Config) -> Self {
        LockVm {
            available: false,
            why: Some("not read yet".into()),
            locked: false,
            has_pin: false,
            auto_lock_min: DEFAULT_AUTO_LOCK_MIN,
            failures: 0,
            retry_at_ms: None,
            pending: None,
            now_ms: 0,
            last_activity_ms: None,
            held: false,
        }
    }

    pub fn state(&self) -> State {
        State {
            available: self.available,
            why: self.why.clone(),
            locked: self.locked,
            has_pin: self.has_pin,
            auto_lock_min: self.auto_lock_min,
            failures: self.failures,
            retry_at_ms: self.retry_at_ms,
        }
    }

    pub fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        match ev {
            Event::Start => {
                self.held = true;
                vec![
                    Effect::Read,
                    Effect::Call(Call::HoldInhibitor { hold: true }),
                    Effect::Call(Call::SetLockedHint { locked: false }),
                ]
            }
            Event::Read(r) => {
                self.has_pin = r.has_pin;
                self.auto_lock_min = r.auto_lock_min;
                self.available = true;
                self.why = None;
                vec![]
            }
            Event::Unavailable { why } => {
                self.available = false;
                self.why = Some(why);
                self.pending = None;
                vec![]
            }
            Event::Intent { verb, args } => self.intent(&verb, args),
            Event::PinChecked { ok } => self.pin_checked(ok),
            Event::PinError { why } => match self.pending.take() {
                Some(p) => vec![refuse(
                    p.verb(),
                    "failed",
                    format!("the PIN could not be checked: {why}"),
                )],
                None => vec![],
            },
            Event::LogindLock => self.lock(),
            Event::LogindUnlock => self.unlock(),
            Event::PrepareForSleep { start } => {
                let mut effects = if start { self.lock() } else { vec![] };
                self.held = !start;
                effects.push(Effect::Call(Call::HoldInhibitor { hold: !start }));
                effects
            }
            Event::Tick { now_ms } => {
                self.now_ms = now_ms;
                let since = *self.last_activity_ms.get_or_insert(now_ms);
                if self.retry_at_ms.is_some_and(|t| now_ms >= t) {
                    self.retry_at_ms = None;
                }
                let idle_ms = u64::from(self.auto_lock_min) * 60_000;
                let mut effects = if self.available
                    && !self.locked
                    && idle_ms > 0
                    && now_ms.saturating_sub(since) >= idle_ms
                {
                    self.lock()
                } else {
                    vec![]
                };
                // a service that was not there is asked again
                if !self.available {
                    effects.push(Effect::Read);
                }
                effects
            }
            Event::Activity { now_ms } => {
                self.now_ms = self.now_ms.max(now_ms);
                let due = self
                    .last_activity_ms
                    .is_none_or(|last| now_ms.saturating_sub(last) >= ACTIVITY_EVERY_MS);
                if due {
                    self.last_activity_ms = Some(now_ms);
                }
                vec![]
            }
        }
    }

    /// Lock the screen, when there is a PIN to unlock it with.
    fn lock(&mut self) -> Vec<Effect> {
        if self.locked || !self.has_pin {
            return vec![];
        }
        self.locked = true;
        vec![Effect::Call(Call::SetLockedHint { locked: true })]
    }

    fn unlock(&mut self) -> Vec<Effect> {
        self.failures = 0;
        self.retry_at_ms = None;
        self.last_activity_ms = Some(self.now_ms);
        if !self.locked {
            return vec![];
        }
        self.locked = false;
        vec![Effect::Call(Call::SetLockedHint { locked: false })]
    }

    fn pin_checked(&mut self, ok: bool) -> Vec<Effect> {
        let Some(pending) = self.pending.take() else {
            return vec![];
        };
        if !ok {
            self.failures += 1;
            if self.failures >= FREE_TRIES {
                self.retry_at_ms = Some(self.now_ms + wait_ms(self.failures));
            }
            let message = match self.retry_at_ms {
                Some(t) => format!(
                    "wrong PIN ({} in a row): wait {} s",
                    self.failures,
                    t.saturating_sub(self.now_ms).div_ceil(1000)
                ),
                None => "wrong PIN".to_string(),
            };
            return vec![refuse(pending.verb(), "wrong_pin", message)];
        }
        self.failures = 0;
        self.retry_at_ms = None;
        match pending {
            Pending::Unlock { .. } => self.unlock(),
            Pending::SetPin { new } => {
                self.has_pin = true;
                vec![self.persist(PinChange::Set(new))]
            }
            Pending::ClearPin => {
                self.has_pin = false;
                // nothing can unlock a screen with no PIN: it opens
                let mut effects = self.unlock();
                effects.insert(0, self.persist(PinChange::Clear));
                effects
            }
        }
    }

    fn persist(&self, pin: PinChange) -> Effect {
        Effect::Call(Call::Persist {
            pin,
            auto_lock_min: self.auto_lock_min,
        })
    }

    fn intent(&mut self, verb: &str, a: Args) -> Vec<Effect> {
        self.now_ms = self.now_ms.max(a.now_ms);
        let refuse = |reason: &str, message: &str| vec![refuse(verb, reason, message.to_string())];
        if !matches!(
            verb,
            "lock" | "unlock" | "set_pin" | "clear_pin" | "set_auto_lock"
        ) {
            return refuse("unsupported", &format!("lock: unknown verb `{verb}`"));
        }
        if !self.available {
            let why = self.why.clone().unwrap_or_default();
            return refuse("unavailable", &format!("{verb}: {why}"));
        }
        if self.pending.is_some() {
            return refuse("busy", &format!("{verb}: a PIN is being checked"));
        }
        match verb {
            "lock" => {
                if !self.has_pin {
                    return refuse(
                        "unsupported",
                        "lock: set a PIN first, or nothing could unlock the screen",
                    );
                }
                self.lock()
            }
            "unlock" => {
                // without a PIN there is nothing to check; unlocked there is nothing to do
                if !self.has_pin || !self.locked {
                    return vec![];
                }
                self.check(
                    Pending::Unlock {
                        verb: "unlock".into(),
                    },
                    a.pin,
                )
            }
            "set_pin" => {
                let Some(new) = a.new.filter(|n| pin_fits(n.expose())) else {
                    return refuse("bad_arg", "set_pin: `new` is 4 to 12 digits");
                };
                if !self.has_pin {
                    self.has_pin = true;
                    return vec![self.persist(PinChange::Set(new))];
                }
                self.check(Pending::SetPin { new }, a.old)
            }
            "clear_pin" => {
                if !self.has_pin {
                    return vec![];
                }
                self.check(Pending::ClearPin, a.pin)
            }
            _ => {
                if a.bad_minutes {
                    return refuse("bad_arg", "set_auto_lock: `minutes` is a whole number");
                }
                match a.minutes {
                    Some(m) if m <= MAX_AUTO_LOCK_MIN => {
                        self.auto_lock_min = m;
                        // the clock starts from now, not from the last input long ago
                        self.last_activity_ms = Some(self.now_ms);
                        vec![self.persist(PinChange::Keep)]
                    }
                    Some(_) => refuse(
                        "bad_arg",
                        &format!("set_auto_lock: `minutes` is 0 (off) to {MAX_AUTO_LOCK_MIN}"),
                    ),
                    None => refuse("bad_arg", "set_auto_lock: needs `minutes`, 0 for off"),
                }
            }
        }
    }

    /// Ask for `pin` to be checked, unless it is malformed or the wait is on.
    fn check(&mut self, what: Pending, pin: Option<Pin>) -> Vec<Effect> {
        let verb = what.verb();
        if let Some(t) = self.retry_at_ms.filter(|t| *t > self.now_ms) {
            return vec![refuse(
                verb,
                "retry_later",
                format!(
                    "{verb}: too many wrong PINs; try again in {} s",
                    (t - self.now_ms).div_ceil(1000)
                ),
            )];
        }
        let Some(pin) = pin.filter(|p| pin_fits(p.expose())) else {
            return vec![refuse(
                verb,
                "bad_arg",
                format!("{verb}: the PIN is 4 to 12 digits"),
            )];
        };
        self.pending = Some(what);
        vec![Effect::CheckPin { pin }]
    }
}

impl Pending {
    fn verb(&self) -> &str {
        match self {
            Pending::Unlock { verb } => verb,
            Pending::SetPin { .. } => "set_pin",
            Pending::ClearPin => "clear_pin",
        }
    }
}

fn refuse(verb: &str, reason: &str, message: String) -> Effect {
    Effect::Refuse {
        verb: verb.into(),
        reason: reason.into(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(s: &str) -> Pin {
        Pin::new(s)
    }

    /// A VM that read its settings: a PIN set, auto-lock at `minutes`.
    fn vm(has_pin: bool, minutes: u32) -> LockVm {
        let mut vm = LockVm::new(Config::default());
        vm.reduce(Event::Start);
        vm.reduce(Event::Read(Read {
            has_pin,
            auto_lock_min: minutes,
        }));
        vm
    }

    fn intent(vm: &mut LockVm, verb: &str, args: Args) -> Vec<Effect> {
        vm.reduce(Event::Intent {
            verb: verb.into(),
            args,
        })
    }

    fn with_pin(p: &str) -> Args {
        Args {
            pin: Some(pin(p)),
            ..Args::default()
        }
    }

    fn hint(locked: bool) -> Effect {
        Effect::Call(Call::SetLockedHint { locked })
    }

    fn reason(effects: &[Effect]) -> &str {
        match &effects[0] {
            Effect::Refuse { reason, .. } => reason,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    /// Lock the screen and try `p` as the PIN at `now`: what the check ended in.
    fn try_unlock(vm: &mut LockVm, p: &str, ok: bool, now: u64) -> Vec<Effect> {
        vm.reduce(Event::Tick { now_ms: now });
        let asked = intent(vm, "unlock", with_pin(p));
        if matches!(&asked[0], Effect::CheckPin { .. }) {
            vm.reduce(Event::PinChecked { ok })
        } else {
            asked
        }
    }

    #[test]
    fn wrong_pin_five_times_sets_retry_at_and_doubles_up_to_fifteen_minutes() {
        let mut vm = vm(true, 0);
        vm.reduce(Event::LogindLock);
        let mut now = 1_000_000u64;
        for n in 1..=4 {
            let r = try_unlock(&mut vm, "0000", false, now);
            assert_eq!(reason(&r), "wrong_pin", "try {n}");
            assert_eq!(
                (vm.state().failures, vm.state().retry_at_ms),
                (n, None),
                "free tries"
            );
            now += 1_000;
        }
        // the fifth: 30 s from now
        let r = try_unlock(&mut vm, "0000", false, now);
        assert!(matches!(&r[0], Effect::Refuse { message, .. } if message.contains("wait 30 s")));
        assert_eq!(vm.state().retry_at_ms, Some(now + 30_000));
        // inside the wait even the right PIN is not looked at
        let r = try_unlock(&mut vm, "1234", true, now + 29_000);
        assert_eq!(reason(&r), "retry_later");
        assert_eq!(vm.state().failures, 5, "a refused attempt is not counted");
        // after it, the next wrong PIN waits twice as long, and so on
        let mut waits = vec![];
        let mut at = now + 30_000;
        for _ in 0..8 {
            try_unlock(&mut vm, "0000", false, at);
            let t = vm.state().retry_at_ms.unwrap();
            waits.push(t - at);
            at = t;
        }
        assert_eq!(
            waits,
            vec![60_000, 120_000, 240_000, 480_000, 900_000, 900_000, 900_000, 900_000]
        );
        assert_eq!(wait_ms(5), 30_000);
        assert_eq!(wait_ms(u32::MAX), MAX_WAIT_MS);
        // the right PIN, once the wait is over, clears all of it
        let r = try_unlock(&mut vm, "1234", true, at);
        assert_eq!(r, vec![hint(false)]);
        let s = vm.state();
        assert_eq!((s.locked, s.failures, s.retry_at_ms), (false, 0, None));
        // the wait ends by itself on a tick
        let mut vm2 = self::vm(true, 0);
        vm2.reduce(Event::LogindLock);
        for _ in 0..5 {
            try_unlock(&mut vm2, "0000", false, 10_000);
        }
        assert!(vm2.state().retry_at_ms.is_some());
        vm2.reduce(Event::Tick {
            now_ms: 10_000 + 30_000,
        });
        assert_eq!(vm2.state().retry_at_ms, None);
        assert_eq!(vm2.state().failures, 5);
    }

    #[test]
    fn the_wait_after_wrong_pins_counts_from_the_verbs_own_clock() {
        // no tick has come yet: the verb brings the time
        let mut vm = vm(true, 0);
        vm.reduce(Event::LogindLock);
        let at = 1_700_000_000_000u64;
        for _ in 0..5 {
            let args = Args {
                now_ms: at,
                ..with_pin("0000")
            };
            intent(&mut vm, "unlock", args);
            vm.reduce(Event::PinChecked { ok: false });
        }
        assert_eq!(vm.state().retry_at_ms, Some(at + 30_000));
    }

    #[test]
    fn unlock_without_a_pin_set_has_no_effect() {
        let mut vm = vm(false, 0);
        assert!(intent(&mut vm, "unlock", with_pin("1234")).is_empty());
        assert!(!vm.state().locked);
        // nothing locks a screen with no PIN: not a verb, not logind, not sleep
        let r = intent(&mut vm, "lock", Args::default());
        assert_eq!(reason(&r), "unsupported");
        assert!(vm.reduce(Event::LogindLock).is_empty());
        assert!(!vm.state().locked);
        let sleep = vm.reduce(Event::PrepareForSleep { start: true });
        assert_eq!(
            sleep,
            vec![Effect::Call(Call::HoldInhibitor { hold: false })]
        );
        assert!(!vm.state().locked);
        // and an unlock of a screen that is not locked asks for nothing
        let mut open = self::vm(true, 0);
        assert!(intent(&mut open, "unlock", with_pin("1234")).is_empty());
    }

    #[test]
    fn correct_pin_unlocks_and_requests_set_locked_hint_false() {
        let mut vm = vm(true, 0);
        assert_eq!(intent(&mut vm, "lock", Args::default()), vec![hint(true)]);
        assert!(vm.state().locked);
        assert_eq!(
            intent(&mut vm, "unlock", with_pin("1234")),
            vec![Effect::CheckPin { pin: pin("1234") }]
        );
        assert!(vm.state().locked, "locked until the check says so");
        assert_eq!(vm.reduce(Event::PinChecked { ok: true }), vec![hint(false)]);
        assert!(!vm.state().locked);
        // logind's own Lock and Unlock do the same
        assert_eq!(vm.reduce(Event::LogindLock), vec![hint(true)]);
        assert_eq!(vm.reduce(Event::LogindUnlock), vec![hint(false)]);
        assert!(vm.reduce(Event::LogindUnlock).is_empty());
        // a malformed PIN is refused without a check or a failure
        vm.reduce(Event::LogindLock);
        let r = intent(&mut vm, "unlock", with_pin("12ab"));
        assert_eq!(reason(&r), "bad_arg");
        assert_eq!(
            reason(&intent(&mut vm, "unlock", Args::default())),
            "bad_arg"
        );
        assert_eq!(vm.state().failures, 0);
    }

    #[test]
    fn pin_must_be_four_to_twelve_digits() {
        for ok in ["1234", "123456", "123456789012"] {
            assert!(pin_fits(ok), "{ok}");
        }
        for bad in ["", "123", "1234567890123", "12 34", "12a4", "١٢٣٤", "-123"] {
            assert!(!pin_fits(bad), "{bad:?}");
        }
        let mut vm = vm(false, 0);
        let set = |new: &str| Args {
            new: Some(pin(new)),
            ..Args::default()
        };
        assert_eq!(reason(&intent(&mut vm, "set_pin", set("123"))), "bad_arg");
        assert_eq!(reason(&intent(&mut vm, "set_pin", set("abcd"))), "bad_arg");
        assert!(!vm.state().has_pin);
        assert_eq!(
            intent(&mut vm, "set_pin", set("1234")),
            vec![Effect::Call(Call::Persist {
                pin: PinChange::Set(pin("1234")),
                auto_lock_min: 0
            })]
        );
        assert!(vm.state().has_pin);
    }

    #[test]
    fn set_pin_requires_old_pin_when_one_exists_and_clear_pin_requires_pin() {
        let mut vm = vm(true, 5);
        let change = |old: Option<&str>, new: &str| Args {
            old: old.map(pin),
            new: Some(pin(new)),
            ..Args::default()
        };
        // no old PIN: refused, and nothing was asked of the port
        assert_eq!(
            reason(&intent(&mut vm, "set_pin", change(None, "4321"))),
            "bad_arg"
        );
        // a wrong one: refused, counted
        assert_eq!(
            intent(&mut vm, "set_pin", change(Some("1111"), "4321")),
            vec![Effect::CheckPin { pin: pin("1111") }]
        );
        assert_eq!(
            reason(&vm.reduce(Event::PinChecked { ok: false })),
            "wrong_pin"
        );
        assert_eq!(vm.state().failures, 1);
        // the right one: persisted, auto-lock minutes kept
        intent(&mut vm, "set_pin", change(Some("1234"), "4321"));
        assert_eq!(
            vm.reduce(Event::PinChecked { ok: true }),
            vec![Effect::Call(Call::Persist {
                pin: PinChange::Set(pin("4321")),
                auto_lock_min: 5
            })]
        );
        assert_eq!(vm.state().failures, 0);
        // clear_pin: the PIN is needed
        assert_eq!(
            reason(&intent(&mut vm, "clear_pin", Args::default())),
            "bad_arg"
        );
        intent(&mut vm, "clear_pin", with_pin("0000"));
        assert_eq!(
            reason(&vm.reduce(Event::PinChecked { ok: false })),
            "wrong_pin"
        );
        assert!(vm.state().has_pin);
        intent(&mut vm, "clear_pin", with_pin("4321"));
        assert_eq!(
            vm.reduce(Event::PinChecked { ok: true }),
            vec![Effect::Call(Call::Persist {
                pin: PinChange::Clear,
                auto_lock_min: 5
            })]
        );
        assert!(!vm.state().has_pin);
        // with none set, clearing is nothing; a second check cannot start while one runs
        assert!(intent(&mut vm, "clear_pin", with_pin("4321")).is_empty());
        let mut busy = self::vm(true, 0);
        intent(&mut busy, "clear_pin", with_pin("1234"));
        assert_eq!(
            reason(&intent(&mut busy, "clear_pin", with_pin("1234"))),
            "busy"
        );
    }

    #[test]
    fn clearing_the_pin_of_a_locked_screen_opens_it() {
        let mut vm = vm(true, 0);
        vm.reduce(Event::LogindLock);
        // the screen is locked, and the right PIN clears: it must not stay locked with no way out
        intent(&mut vm, "clear_pin", with_pin("1234"));
        let effects = vm.reduce(Event::PinChecked { ok: true });
        assert_eq!(
            effects,
            vec![
                Effect::Call(Call::Persist {
                    pin: PinChange::Clear,
                    auto_lock_min: 0
                }),
                hint(false)
            ]
        );
        assert!(!vm.state().locked && !vm.state().has_pin);
    }

    #[test]
    fn auto_lock_fires_after_idle_minutes_by_tick_and_zero_is_off() {
        let mut vm = vm(true, 2);
        assert!(
            vm.reduce(Event::Tick { now_ms: 1_000 }).is_empty(),
            "the clock starts at the first tick"
        );
        assert!(vm.reduce(Event::Tick { now_ms: 120_999 }).is_empty());
        assert_eq!(vm.reduce(Event::Tick { now_ms: 121_000 }), vec![hint(true)]);
        assert!(vm.state().locked);
        assert!(
            vm.reduce(Event::Tick { now_ms: 999_000 }).is_empty(),
            "already locked"
        );
        // activity moves the deadline
        let mut vm = self::vm(true, 2);
        vm.reduce(Event::Tick { now_ms: 0 });
        vm.reduce(Event::Activity { now_ms: 100_000 });
        assert!(vm.reduce(Event::Tick { now_ms: 120_000 }).is_empty());
        assert_eq!(vm.reduce(Event::Tick { now_ms: 220_000 }), vec![hint(true)]);
        // 0 is off, and so is having no PIN
        let mut off = self::vm(true, 0);
        off.reduce(Event::Tick { now_ms: 0 });
        assert!(off.reduce(Event::Tick { now_ms: 86_400_000 }).is_empty());
        let mut nopin = self::vm(false, 1);
        nopin.reduce(Event::Tick { now_ms: 0 });
        assert!(nopin.reduce(Event::Tick { now_ms: 600_000 }).is_empty());
        // set_auto_lock: the setting is persisted, 0 turns it off, junk is refused
        let mut vm = self::vm(true, 10);
        vm.reduce(Event::Tick { now_ms: 5_000 });
        let minutes = |m| Args {
            minutes: Some(m),
            ..Args::default()
        };
        assert_eq!(
            intent(&mut vm, "set_auto_lock", minutes(0)),
            vec![Effect::Call(Call::Persist {
                pin: PinChange::Keep,
                auto_lock_min: 0
            })]
        );
        assert_eq!(vm.state().auto_lock_min, 0);
        assert_eq!(
            reason(&intent(&mut vm, "set_auto_lock", minutes(1_441))),
            "bad_arg"
        );
        assert_eq!(
            reason(&intent(&mut vm, "set_auto_lock", Args::default())),
            "bad_arg"
        );
        let bad = Args {
            bad_minutes: true,
            ..Args::default()
        };
        assert_eq!(reason(&intent(&mut vm, "set_auto_lock", bad)), "bad_arg");
        assert_eq!(vm.state().auto_lock_min, 0);
    }

    #[test]
    fn activity_is_throttled_to_once_per_ten_seconds() {
        // 1 minute of auto-lock; the clock starts at the tick at 0
        let mut vm = vm(true, 1);
        vm.reduce(Event::Tick { now_ms: 0 });
        vm.reduce(Event::Activity { now_ms: 5_000 }); // 5 s after the last: dropped
        vm.reduce(Event::Activity { now_ms: 12_000 }); // taken
        vm.reduce(Event::Activity { now_ms: 21_999 }); // 9.999 s later: dropped
                                                       // the last taken input was at 12 s: the lock is due at 72 s, not 82 s
        assert!(vm.reduce(Event::Tick { now_ms: 71_999 }).is_empty());
        assert_eq!(vm.reduce(Event::Tick { now_ms: 72_000 }), vec![hint(true)]);
        let mut vm = self::vm(true, 1);
        vm.reduce(Event::Tick { now_ms: 0 });
        vm.reduce(Event::Activity { now_ms: 12_000 });
        vm.reduce(Event::Activity { now_ms: 22_000 }); // exactly 10 s later: taken
        assert!(vm.reduce(Event::Tick { now_ms: 81_999 }).is_empty());
        assert_eq!(vm.reduce(Event::Tick { now_ms: 82_000 }), vec![hint(true)]);
    }

    #[test]
    fn prepare_for_sleep_locks_and_releases_then_retakes_inhibitor() {
        let mut vm = vm(true, 0);
        assert_eq!(
            vm.reduce(Event::PrepareForSleep { start: true }),
            vec![
                hint(true),
                Effect::Call(Call::HoldInhibitor { hold: false })
            ]
        );
        assert!(vm.state().locked);
        assert_eq!(
            vm.reduce(Event::PrepareForSleep { start: false }),
            vec![Effect::Call(Call::HoldInhibitor { hold: true })]
        );
        assert!(vm.state().locked, "it stays locked after waking");
        // the first event of a boot takes the inhibitor and tells logind it is unlocked
        let mut fresh = LockVm::new(Config::default());
        assert_eq!(
            fresh.reduce(Event::Start),
            vec![
                Effect::Read,
                Effect::Call(Call::HoldInhibitor { hold: true }),
                hint(false)
            ]
        );
    }

    #[test]
    fn unavailable_until_read_and_asked_again_on_a_tick() {
        let mut vm = LockVm::new(Config::default());
        let s = vm.state();
        assert_eq!(
            (s.available, s.why.as_deref()),
            (false, Some("not read yet"))
        );
        assert_eq!(
            reason(&intent(&mut vm, "lock", Args::default())),
            "unavailable"
        );
        vm.reduce(Event::Unavailable {
            why: "org.freedesktop.login1 is not on the system bus".into(),
        });
        assert_eq!(vm.reduce(Event::Tick { now_ms: 5_000 }), vec![Effect::Read]);
        vm.reduce(Event::Read(Read {
            has_pin: true,
            auto_lock_min: 10,
        }));
        assert!(vm.state().available && vm.state().why.is_none());
        assert!(vm.reduce(Event::Tick { now_ms: 10_000 }).is_empty());
        assert_eq!(
            reason(&intent(&mut vm, "nope", Args::default())),
            "unsupported"
        );
    }

    #[test]
    fn the_pin_never_prints_and_the_state_is_the_documented_json() {
        let secret = "9876";
        let mut vm = vm(true, 10);
        vm.reduce(Event::LogindLock);
        let ev = Event::Intent {
            verb: "unlock".into(),
            args: with_pin(secret),
        };
        assert!(!format!("{ev:?}").contains(secret));
        let effects = vm.reduce(ev);
        assert!(!format!("{effects:?}").contains(secret), "{effects:?}");
        assert!(!format!("{:?}", vm.state()).contains(secret));
        assert!(!serde_json::to_string(&vm.state()).unwrap().contains(secret));
        let json = serde_json::to_value(vm.state()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"available": true, "locked": true, "has_pin": true,
                               "auto_lock_min": 10, "failures": 0, "retry_at_ms": null})
        );
    }
}
