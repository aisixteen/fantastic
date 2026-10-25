//! THE DISPLAYS AGENT: the outputs of the Wayland compositor (`wlr-randr`) and
//! the backlight (logind's `SetBrightness`, `/sys/class/backlight`). The sys
//! layer runs `wlr-randr --json` and reads the backlight directory and hands
//! both over as a [`Read`]; this module reads the JSON, builds the
//! `wlr-randr` arguments of a change and decides every refusal.

use serde::Serialize;
use serde_json::Value;

/// The transforms `wlr-randr --transform` takes.
pub const TRANSFORMS: [&str; 8] = [
    "normal",
    "90",
    "180",
    "270",
    "flipped",
    "flipped-90",
    "flipped-180",
    "flipped-270",
];

/// A refresh rate a user names matches a mode within this many mHz.
const REFRESH_SLACK_MHZ: u32 = 1_000;

/// One backlight device as `/sys/class/backlight/<name>` has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BacklightRead {
    pub name: String,
    pub brightness: u32,
    pub max: u32,
    /// The `type` file: `raw`, `platform` or `firmware`; empty when unknown.
    pub kind: String,
}

/// One read of the machine.
#[derive(Clone, Debug, PartialEq)]
pub struct Read {
    /// `wlr-randr --json`, parsed.
    pub outputs: Value,
    pub backlights: Vec<BacklightRead>,
}

/// A mode a verb names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeArg {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: Option<u32>,
}

impl ModeArg {
    /// `WxH` or `WxH@R` (R in Hz, `Hz` suffix allowed).
    pub fn parse(s: &str) -> Option<ModeArg> {
        let (size, refresh) = match s.split_once('@') {
            Some((size, r)) => (size, Some(r)),
            None => (s, None),
        };
        let (w, h) = size.split_once('x')?;
        let refresh_mhz = match refresh {
            None => None,
            Some(r) => {
                let hz: f64 = r.trim_end_matches("Hz").trim().parse().ok()?;
                if !hz.is_finite() || hz <= 0.0 {
                    return None;
                }
                Some((hz * 1000.0).round() as u32)
            }
        };
        Some(ModeArg {
            width: w.trim().parse().ok()?,
            height: h.trim().parse().ok()?,
            refresh_mhz,
        })
    }
}

/// What a verb brings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Args {
    pub name: Option<String>,
    pub mode: Option<ModeArg>,
    /// A `mode` the verb sent that is no mode at all.
    pub bad_mode: bool,
    pub scale: Option<f64>,
    pub transform: Option<String>,
    pub position: Option<(i32, i32)>,
    pub enabled: Option<bool>,
    pub value: Option<f64>,
}

/// A change to make.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    /// Run `wlr-randr` with these arguments (they start with `--output <name>`).
    SetOutput { name: String, argv: Vec<String> },
    /// logind `Session.SetBrightness("backlight", name, brightness)`.
    SetBrightness { name: String, brightness: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The first event of a boot: read everything.
    Start,
    /// An answer to [`Effect::Read`].
    Read(Read),
    /// A change went through: read again.
    Changed,
    /// A verb: `set_output` or `set_brightness`.
    Intent { verb: String, args: Args },
    /// The compositor or the tool is not there (or a read failed).
    Unavailable { why: String },
    /// Time: `wlr-randr` has no change stream, so every tick reads again.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ModeInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub preferred: bool,
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Output {
    pub name: String,
    pub enabled: bool,
    pub make: String,
    pub model: String,
    /// The current mode; `null` when the output is off.
    pub mode: Option<Mode>,
    pub modes: Vec<ModeInfo>,
    pub scale: f64,
    pub transform: String,
    pub position: [i32; 2],
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Backlight {
    pub name: String,
    /// 0.0..=1.0, two places.
    pub value: f64,
}

/// What the view draws; its JSON is the agent's `state` reply.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct State {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub outputs: Vec<Output>,
    pub backlight: Option<Backlight>,
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn mhz(refresh_hz: f64) -> u32 {
    (refresh_hz * 1000.0).round().max(0.0) as u32
}

/// `wlr-randr --json` as outputs. An entry with no name is skipped.
pub fn parse_outputs(json: &Value) -> Vec<Output> {
    let mut out = Vec::new();
    for o in json.as_array().into_iter().flatten() {
        let name = text(o, "name");
        if name.is_empty() {
            continue;
        }
        let modes: Vec<ModeInfo> = o
            .get("modes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| {
                Some(ModeInfo {
                    width: u32::try_from(m.get("width")?.as_u64()?).ok()?,
                    height: u32::try_from(m.get("height")?.as_u64()?).ok()?,
                    refresh_mhz: mhz(m.get("refresh").and_then(Value::as_f64).unwrap_or(0.0)),
                    preferred: m.get("preferred").and_then(Value::as_bool).unwrap_or(false),
                    current: m.get("current").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect();
        let enabled = o.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let mode = modes.iter().find(|m| m.current).map(|m| Mode {
            width: m.width,
            height: m.height,
            refresh_mhz: m.refresh_mhz,
        });
        // a position is `{x, y}`
        let coord = |k: &str| {
            o.get("position")
                .and_then(|p| p.get(k))
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(0)
        };
        out.push(Output {
            name,
            enabled,
            make: text(o, "make"),
            model: text(o, "model"),
            mode,
            modes,
            scale: o.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
            transform: o
                .get("transform")
                .and_then(Value::as_str)
                .unwrap_or("normal")
                .to_string(),
            position: [coord("x"), coord("y")],
        });
    }
    out
}

/// The backlight the agent drives: `raw` over `platform` over `firmware`, and
/// the first device when the kinds do not tell them apart.
fn pick(devices: &[BacklightRead]) -> Option<&BacklightRead> {
    let rank = |d: &BacklightRead| match d.kind.as_str() {
        "raw" => 0,
        "platform" => 1,
        "firmware" => 2,
        _ => 3,
    };
    devices
        .iter()
        .enumerate()
        .min_by_key(|(i, d)| (rank(d), *i))
        .map(|(_, d)| d)
}

/// A mode's refresh as `wlr-randr` takes it: Hz, without trailing zeros.
fn hz(mhz: u32) -> String {
    let s = format!("{}.{:03}", mhz / 1000, mhz % 1000);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub struct DisplaysVm {
    available: bool,
    why: Option<String>,
    outputs: Vec<Output>,
    backlights: Vec<BacklightRead>,
}

impl DisplaysVm {
    pub fn new(_cfg: Config) -> Self {
        DisplaysVm {
            available: false,
            why: Some("not read yet".into()),
            outputs: vec![],
            backlights: vec![],
        }
    }

    pub fn state(&self) -> State {
        State {
            available: self.available,
            why: self.why.clone(),
            outputs: self.outputs.clone(),
            backlight: pick(&self.backlights).map(|d| Backlight {
                name: d.name.clone(),
                value: if d.max == 0 {
                    0.0
                } else {
                    (f64::from(d.brightness.min(d.max)) / f64::from(d.max) * 100.0).round() / 100.0
                },
            }),
        }
    }

    pub fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        match ev {
            Event::Start | Event::Changed | Event::Tick { .. } => vec![Effect::Read],
            Event::Read(r) => {
                self.outputs = parse_outputs(&r.outputs);
                self.backlights = r.backlights;
                self.available = true;
                self.why = None;
                vec![]
            }
            Event::Unavailable { why } => {
                self.available = false;
                self.why = Some(why);
                self.outputs.clear();
                self.backlights.clear();
                vec![]
            }
            Event::Intent { verb, args } => vec![self.intent(&verb, &args)],
        }
    }

    fn intent(&self, verb: &str, a: &Args) -> Effect {
        let refuse = |reason: &str, message: String| Effect::Refuse {
            verb: verb.into(),
            reason: reason.into(),
            message,
        };
        if !matches!(verb, "set_output" | "set_brightness") {
            return refuse("unsupported", format!("displays: unknown verb `{verb}`"));
        }
        if !self.available {
            let why = self.why.clone().unwrap_or_default();
            return refuse("unavailable", format!("{verb}: {why}"));
        }
        if verb == "set_brightness" {
            let Some(v) = a.value.filter(|v| v.is_finite()) else {
                return refuse(
                    "bad_arg",
                    "set_brightness: needs a number `value`, 0.0 to 1.0".into(),
                );
            };
            let Some(dev) = pick(&self.backlights) else {
                return refuse(
                    "unsupported",
                    "set_brightness: this machine has no backlight".into(),
                );
            };
            let brightness = (v.clamp(0.0, 1.0) * f64::from(dev.max)).round() as u32;
            return Effect::Call(Call::SetBrightness {
                name: dev.name.clone(),
                brightness,
            });
        }
        self.set_output(a, refuse)
    }

    fn set_output(&self, a: &Args, refuse: impl Fn(&str, String) -> Effect) -> Effect {
        let Some(name) = a.name.as_deref() else {
            return refuse(
                "bad_arg",
                "set_output: needs the `name` of an output".into(),
            );
        };
        let Some(out) = self.outputs.iter().find(|o| o.name == name) else {
            return refuse("not_found", format!("set_output: no output `{name}`"));
        };
        let mut argv = vec!["--output".to_string(), name.to_string()];
        if a.bad_mode {
            return refuse("bad_arg", "set_output: `mode` is WxH or WxH@R".into());
        }
        if let Some(m) = a.mode {
            let found = out.modes.iter().find(|x| {
                x.width == m.width
                    && x.height == m.height
                    && m.refresh_mhz
                        .is_none_or(|r| x.refresh_mhz.abs_diff(r) < REFRESH_SLACK_MHZ)
            });
            let Some(found) = found else {
                return refuse(
                    "bad_arg",
                    format!("set_output: `{name}` has no {}x{} mode", m.width, m.height),
                );
            };
            argv.push("--mode".into());
            argv.push(match m.refresh_mhz {
                Some(r) => format!("{}x{}@{}", m.width, m.height, hz(r)),
                None => format!("{}x{}", found.width, found.height),
            });
        }
        if let Some(s) = a.scale {
            if !s.is_finite() || s <= 0.0 {
                return refuse("bad_arg", "set_output: `scale` is a number above 0".into());
            }
            argv.push("--scale".into());
            argv.push(s.to_string());
        }
        if let Some(t) = a.transform.as_deref() {
            if !TRANSFORMS.contains(&t) {
                return refuse(
                    "bad_arg",
                    format!(
                        "set_output: `transform` is one of {}",
                        TRANSFORMS.join(", ")
                    ),
                );
            }
            argv.push("--transform".into());
            argv.push(t.into());
        }
        if let Some((x, y)) = a.position {
            argv.push("--pos".into());
            argv.push(format!("{x},{y}"));
        }
        if let Some(on) = a.enabled {
            if !on && out.enabled && self.outputs.iter().filter(|o| o.enabled).count() == 1 {
                return refuse(
                    "unsupported",
                    format!("set_output: `{name}` is the last enabled output; turning it off would leave no screen"),
                );
            }
            argv.push(if on { "--on" } else { "--off" }.into());
        }
        if argv.len() == 2 {
            return refuse(
                "bad_arg",
                "set_output: nothing to change (`mode`, `scale`, `transform`, `position` or `enabled`)".into(),
            );
        }
        Effect::Call(Call::SetOutput {
            name: name.into(),
            argv,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/wlr_randr.json");

    fn read(backlights: Vec<BacklightRead>) -> Read {
        Read {
            outputs: serde_json::from_str(FIXTURE).unwrap(),
            backlights,
        }
    }

    fn light(name: &str, brightness: u32, max: u32, kind: &str) -> BacklightRead {
        BacklightRead {
            name: name.into(),
            brightness,
            max,
            kind: kind.into(),
        }
    }

    fn vm_with(r: Read) -> DisplaysVm {
        let mut vm = DisplaysVm::new(Config::default());
        assert_eq!(vm.reduce(Event::Start), vec![Effect::Read]);
        vm.reduce(Event::Read(r));
        vm
    }

    fn named(name: &str) -> Args {
        Args {
            name: Some(name.into()),
            ..Args::default()
        }
    }

    fn intent(vm: &mut DisplaysVm, verb: &str, args: Args) -> Effect {
        vm.reduce(Event::Intent {
            verb: verb.into(),
            args,
        })
        .remove(0)
    }

    fn argv(e: Effect) -> Vec<String> {
        match e {
            Effect::Call(Call::SetOutput { argv, .. }) => argv,
            other => panic!("not a set_output call: {other:?}"),
        }
    }

    fn reason(e: &Effect) -> &str {
        match e {
            Effect::Refuse { reason, .. } => reason,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    #[test]
    fn wlr_randr_fixture_parses_outputs_modes_and_current_mode() {
        let s = vm_with(read(vec![])).state();
        assert!(s.available && s.why.is_none());
        assert_eq!(s.outputs.len(), 2);
        let v = &s.outputs[0];
        assert_eq!(
            (v.name.as_str(), v.enabled, v.scale),
            ("Virtual-1", true, 1.0)
        );
        assert_eq!(
            v.mode,
            Some(Mode {
                width: 1280,
                height: 800,
                refresh_mhz: 59_999
            })
        );
        assert_eq!(v.modes.len(), 3);
        assert!(v.modes[0].preferred && v.modes[0].current && !v.modes[1].current);
        assert_eq!(v.modes[2].refresh_mhz, 60_004);
        assert_eq!((v.transform.as_str(), v.position), ("normal", [0, 0]));
        // an output that is off: no current mode, defaults for what it omits
        let h = &s.outputs[1];
        assert_eq!(
            (h.make.as_str(), h.model.as_str(), h.enabled),
            ("Acme", "Panel 24", false)
        );
        assert_eq!(h.mode, None);
        assert_eq!(
            (h.scale, h.transform.as_str(), h.position),
            (1.0, "normal", [0, 0])
        );
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["outputs"][0]["mode"]["refresh_mhz"], 59_999);
        assert!(json["backlight"].is_null());
        // an entry with no name is skipped; a non-array is nothing
        assert_eq!(
            parse_outputs(&serde_json::json!([{"enabled": true}])),
            vec![]
        );
        assert_eq!(parse_outputs(&serde_json::json!({})), vec![]);
    }

    #[test]
    fn set_output_builds_the_wlr_randr_arguments() {
        let mut vm = vm_with(read(vec![]));
        let all = Args {
            mode: Some(ModeArg {
                width: 1280,
                height: 720,
                refresh_mhz: Some(60_000),
            }),
            scale: Some(1.5),
            transform: Some("flipped-90".into()),
            position: Some((1280, -10)),
            enabled: Some(true),
            ..named("Virtual-1")
        };
        assert_eq!(
            argv(intent(&mut vm, "set_output", all)),
            [
                "--output",
                "Virtual-1",
                "--mode",
                "1280x720@60",
                "--scale",
                "1.5",
                "--transform",
                "flipped-90",
                "--pos",
                "1280,-10",
                "--on"
            ]
        );
        let only_mode = Args {
            mode: ModeArg::parse("1024x768"),
            ..named("Virtual-1")
        };
        assert_eq!(
            argv(intent(&mut vm, "set_output", only_mode)),
            ["--output", "Virtual-1", "--mode", "1024x768"]
        );
        let fractional = Args {
            mode: ModeArg::parse("1024x768@60.004Hz"),
            ..named("Virtual-1")
        };
        assert_eq!(
            argv(intent(&mut vm, "set_output", fractional))[3],
            "1024x768@60.004"
        );
        let on = Args {
            enabled: Some(true),
            ..named("HDMI-A-1")
        };
        assert_eq!(
            argv(intent(&mut vm, "set_output", on)),
            ["--output", "HDMI-A-1", "--on"]
        );
        // the refusals
        let bad =
            |args| reason(&intent(&mut vm_with(read(vec![])), "set_output", args)).to_string();
        assert_eq!(bad(Args::default()), "bad_arg");
        assert_eq!(bad(named("nope")), "not_found");
        assert_eq!(bad(named("Virtual-1")), "bad_arg", "nothing to change");
        assert_eq!(
            bad(Args {
                mode: ModeArg::parse("640x480"),
                ..named("Virtual-1")
            }),
            "bad_arg"
        );
        assert_eq!(
            bad(Args {
                mode: ModeArg::parse("1280x720@75"),
                ..named("Virtual-1")
            }),
            "bad_arg"
        );
        assert_eq!(
            bad(Args {
                bad_mode: true,
                ..named("Virtual-1")
            }),
            "bad_arg"
        );
        assert_eq!(
            bad(Args {
                transform: Some("upside-down".into()),
                ..named("Virtual-1")
            }),
            "bad_arg"
        );
        for scale in [0.0, -1.0, f64::NAN] {
            assert_eq!(
                bad(Args {
                    scale: Some(scale),
                    ..named("Virtual-1")
                }),
                "bad_arg"
            );
        }
        assert_eq!(ModeArg::parse("wide"), None);
        assert_eq!(ModeArg::parse("1x2@fast"), None);
    }

    #[test]
    fn turning_the_last_enabled_output_off_is_refused() {
        let mut vm = vm_with(read(vec![]));
        let off = |name: &str| Args {
            enabled: Some(false),
            ..named(name)
        };
        // Virtual-1 is the only one on
        let e = intent(&mut vm, "set_output", off("Virtual-1"));
        assert_eq!(reason(&e), "unsupported");
        assert!(
            matches!(&e, Effect::Refuse { message, .. } if message.contains("last enabled output"))
        );
        // turning an output that is already off, off again, is not that
        assert_eq!(
            argv(intent(&mut vm, "set_output", off("HDMI-A-1"))),
            ["--output", "HDMI-A-1", "--off"]
        );
        // with a second output on, either may go
        let mut r = read(vec![]);
        r.outputs[1]["enabled"] = serde_json::json!(true);
        let mut vm = vm_with(r);
        assert_eq!(
            argv(intent(&mut vm, "set_output", off("Virtual-1"))),
            ["--output", "Virtual-1", "--off"]
        );
    }

    #[test]
    fn brightness_is_clamped_to_zero_one() {
        let mut vm = vm_with(read(vec![light("intel_backlight", 600, 1000, "raw")]));
        let set = |v: f64| Args {
            value: Some(v),
            ..Args::default()
        };
        let call = |vm: &mut DisplaysVm, v: f64| match intent(vm, "set_brightness", set(v)) {
            Effect::Call(Call::SetBrightness { name, brightness }) => {
                assert_eq!(name, "intel_backlight");
                brightness
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(call(&mut vm, 0.25), 250);
        assert_eq!(call(&mut vm, 1.0), 1000);
        assert_eq!(call(&mut vm, 7.0), 1000, "above 1 is 1");
        assert_eq!(call(&mut vm, -3.0), 0, "below 0 is 0");
        assert_eq!(call(&mut vm, 0.0004), 0);
        assert_eq!(
            reason(&intent(&mut vm, "set_brightness", Args::default())),
            "bad_arg"
        );
        assert_eq!(
            reason(&intent(&mut vm, "set_brightness", set(f64::NAN))),
            "bad_arg"
        );
        assert_eq!(vm.state().backlight.unwrap().value, 0.6);
    }

    #[test]
    fn backlight_is_null_without_one_and_falls_back_to_the_first_device() {
        let mut vm = vm_with(read(vec![]));
        assert!(vm.state().backlight.is_none());
        let none = Args {
            value: Some(0.5),
            ..Args::default()
        };
        assert_eq!(
            reason(&intent(&mut vm, "set_brightness", none)),
            "unsupported"
        );
        // kinds unknown: the first device
        let vm = vm_with(read(vec![
            light("acpi_video0", 5, 10, ""),
            light("intel_backlight", 900, 1000, ""),
        ]));
        let b = vm.state().backlight.unwrap();
        assert_eq!((b.name.as_str(), b.value), ("acpi_video0", 0.5));
        // raw over platform over firmware, whatever the order
        let vm = vm_with(read(vec![
            light("acpi_video0", 5, 10, "firmware"),
            light("thinkpad_screen", 1, 10, "platform"),
            light("intel_backlight", 900, 1000, "raw"),
        ]));
        assert_eq!(vm.state().backlight.unwrap().name, "intel_backlight");
        // a max of 0 is 0.0, not a division by zero; a value above max is 1
        let vm = vm_with(read(vec![light("odd", 3, 0, "raw")]));
        assert_eq!(vm.state().backlight.unwrap().value, 0.0);
        let vm = vm_with(read(vec![light("hot", 20, 10, "raw")]));
        assert_eq!(vm.state().backlight.unwrap().value, 1.0);
    }

    #[test]
    fn state_changes_only_when_the_read_differs() {
        let mut vm = vm_with(read(vec![light("b", 5, 10, "raw")]));
        let first = vm.state();
        vm.reduce(Event::Read(read(vec![light("b", 5, 10, "raw")])));
        assert_eq!(vm.state(), first, "an identical read is an identical state");
        let mut changed = read(vec![light("b", 5, 10, "raw")]);
        changed.outputs[0]["scale"] = serde_json::json!(2.0);
        vm.reduce(Event::Read(changed));
        assert_ne!(vm.state(), first);
        // every tick and every change asks for a read
        assert_eq!(vm.reduce(Event::Tick { now_ms: 5_000 }), vec![Effect::Read]);
        assert_eq!(vm.reduce(Event::Changed), vec![Effect::Read]);
    }

    #[test]
    fn unavailable_before_a_read_and_after_a_failure() {
        let mut vm = DisplaysVm::new(Config::default());
        let s = vm.state();
        assert_eq!(
            (s.available, s.why.as_deref()),
            (false, Some("not read yet"))
        );
        assert_eq!(
            reason(&intent(&mut vm, "set_brightness", Args::default())),
            "unavailable"
        );
        vm.reduce(Event::Read(read(vec![])));
        let why = "no Wayland display at /run/user/1000/wayland-0";
        vm.reduce(Event::Unavailable { why: why.into() });
        let s = vm.state();
        assert!(!s.available && s.outputs.is_empty());
        assert_eq!(s.why.as_deref(), Some(why));
        assert_eq!(
            reason(&intent(&mut vm, "set_output", named("Virtual-1"))),
            "unavailable"
        );
        assert_eq!(
            reason(&intent(&mut vm, "nope", Args::default())),
            "unsupported"
        );
    }
}
