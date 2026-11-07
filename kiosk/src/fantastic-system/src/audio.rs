//! THE AUDIO AGENT: PipeWire's sinks and sources, their volume and mute, and
//! the default of each. The sys layer hands over `pw-dump`'s JSON as it is (a
//! whole dump, or one `--monitor` array of changed objects) and runs `wpctl`;
//! this module reads the JSON and decides everything else.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// The highest volume a verb may set (`wpctl set-volume … -l 1.5`).
pub const MAX_VOLUME: f64 = 1.5;

const NODE: &str = "PipeWire:Interface:Node";
const METADATA: &str = "PipeWire:Interface:Metadata";
const DEFAULT_SINK: &str = "default.audio.sink";
const DEFAULT_SOURCE: &str = "default.audio.source";

#[derive(Clone, Debug, Default)]
pub struct Config {}

/// A `wpctl` call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Call {
    SetVolume { id: u32, volume: f64 },
    SetMute { id: u32, muted: bool },
    SetDefault { id: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The first event of a boot: read everything.
    Start,
    /// An answer to [`Effect::Read`]: a whole `pw-dump` array.
    Dump(Value),
    /// One `pw-dump --monitor` array: the objects that changed.
    Delta(Value),
    /// A call went through: read again.
    Changed,
    /// PipeWire or the tools are not there (or a read failed).
    Unavailable { why: String },
    /// Time; audio does not poll while available, and re-reads while unavailable.
    Tick { now_ms: u64 },
    /// A verb: `set_volume`, `set_mute` or `set_default`.
    Intent {
        verb: String,
        id: Option<u32>,
        volume: Option<f64>,
        muted: Option<bool>,
    },
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

/// One sink or source, as the view draws it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Device {
    pub id: u32,
    pub name: String,
    pub description: String,
    /// The cube root of the mean channel volume (what `wpctl` shows), two places.
    pub volume: f64,
    pub muted: bool,
    pub default: bool,
}

/// What the view draws; its JSON is the agent's `state` reply.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct State {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub sinks: Vec<Device>,
    pub sources: Vec<Device>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Sink,
    Source,
}

/// A node's raw data, merged across deltas.
#[derive(Clone, Debug, PartialEq)]
struct Node {
    class: Class,
    name: String,
    description: Option<String>,
    nick: Option<String>,
    channel_volumes: Vec<f64>,
    muted: bool,
}

pub struct AudioVm {
    available: bool,
    why: Option<String>,
    nodes: BTreeMap<u32, Node>,
    /// The id of the `default` metadata object, once seen.
    metadata_id: Option<u32>,
    default_sink: Option<String>,
    default_source: Option<String>,
}

/// `media.class` as a class this agent keeps.
fn class_of(props: &Value) -> Option<Class> {
    match props.get("media.class").and_then(Value::as_str) {
        Some("Audio/Sink") => Some(Class::Sink),
        Some("Audio/Source") => Some(Class::Source),
        _ => None,
    }
}

fn text(props: &Value, key: &str) -> Option<String> {
    props.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The cube root of the mean, two places; 0.0 with no channels.
pub fn volume_of(channel_volumes: &[f64]) -> f64 {
    if channel_volumes.is_empty() {
        return 0.0;
    }
    let mean = channel_volumes.iter().sum::<f64>() / channel_volumes.len() as f64;
    round2(mean.cbrt())
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// `info.params.Props[0]`: the channel volumes and mute, when the object has them.
fn props_param(info: &Value) -> Option<(Vec<f64>, bool)> {
    let p = info.get("params")?.get("Props")?.get(0)?;
    let volumes = p
        .get("channelVolumes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    let muted = p.get("mute").and_then(Value::as_bool).unwrap_or(false);
    Some((volumes, muted))
}

/// A default metadata entry's `value.name`: an object, or the same as a JSON string.
fn default_name(value: &Value) -> Option<String> {
    match value {
        Value::Object(_) => text(value, "name"),
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| text(&v, "name")),
        _ => None,
    }
}

impl AudioVm {
    pub fn new(_cfg: Config) -> Self {
        AudioVm {
            available: false,
            why: Some("not read yet".into()),
            nodes: BTreeMap::new(),
            metadata_id: None,
            default_sink: None,
            default_source: None,
        }
    }

    pub fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        match ev {
            Event::Start | Event::Changed => vec![Effect::Read],
            // PipeWire may come up late or a read may time out: ask again until a dump lands
            Event::Tick { .. } if !self.available => vec![Effect::Read],
            Event::Tick { .. } => vec![],
            Event::Dump(v) => {
                self.nodes.clear();
                self.metadata_id = None;
                self.default_sink = None;
                self.default_source = None;
                self.apply(&v);
                self.available = true;
                self.why = None;
                vec![]
            }
            Event::Delta(v) => {
                self.apply(&v);
                vec![]
            }
            Event::Unavailable { why } => {
                self.available = false;
                self.why = Some(why);
                vec![]
            }
            Event::Intent {
                verb,
                id,
                volume,
                muted,
            } => vec![self.intent(&verb, id, volume, muted)],
        }
    }

    pub fn state(&self) -> State {
        let device = |(id, n): (&u32, &Node)| {
            let default = match n.class {
                Class::Sink => &self.default_sink,
                Class::Source => &self.default_source,
            };
            Device {
                id: *id,
                name: n.name.clone(),
                description: n
                    .description
                    .clone()
                    .or_else(|| n.nick.clone())
                    .unwrap_or_else(|| n.name.clone()),
                volume: volume_of(&n.channel_volumes),
                muted: n.muted,
                default: default.as_deref() == Some(n.name.as_str()),
            }
        };
        let of = |class: Class| {
            self.nodes
                .iter()
                .filter(|(_, n)| n.class == class)
                .map(device)
                .collect()
        };
        State {
            available: self.available,
            why: self.why.clone(),
            sinks: of(Class::Sink),
            sources: of(Class::Source),
        }
    }

    /// Fold one `pw-dump` array (a dump or a delta) into the nodes and defaults.
    fn apply(&mut self, v: &Value) {
        for obj in v.as_array().into_iter().flatten() {
            let Some(id) = obj
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|i| u32::try_from(i).ok())
            else {
                continue;
            };
            if obj.get("info").is_some_and(Value::is_null) {
                self.nodes.remove(&id);
                if self.metadata_id == Some(id) {
                    (self.metadata_id, self.default_sink, self.default_source) = (None, None, None);
                }
                continue;
            }
            let kind = obj.get("type").and_then(Value::as_str);
            let is_default_meta = kind == Some(METADATA)
                && (self.metadata_id == Some(id)
                    || obj
                        .get("props")
                        .and_then(|p| p.get("metadata.name"))
                        .and_then(Value::as_str)
                        == Some("default"));
            if is_default_meta {
                self.metadata_id = Some(id);
                self.apply_metadata(obj);
            } else if kind == Some(NODE) || (kind.is_none() && self.nodes.contains_key(&id)) {
                self.apply_node(id, obj);
            }
        }
    }

    fn apply_metadata(&mut self, obj: &Value) {
        for entry in obj
            .get("metadata")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let slot = match entry.get("key").and_then(Value::as_str) {
                Some(DEFAULT_SINK) => &mut self.default_sink,
                Some(DEFAULT_SOURCE) => &mut self.default_source,
                _ => continue,
            };
            *slot = entry.get("value").and_then(default_name);
        }
    }

    /// Merge a node: what the object omits (props, params) stays as it was.
    fn apply_node(&mut self, id: u32, obj: &Value) {
        let Some(info) = obj.get("info") else { return };
        if let Some(props) = info.get("props") {
            match class_of(props) {
                Some(class) => {
                    let node = self.nodes.entry(id).or_insert_with(|| Node {
                        class,
                        name: String::new(),
                        description: None,
                        nick: None,
                        channel_volumes: vec![],
                        muted: false,
                    });
                    node.class = class;
                    if let Some(name) = text(props, "node.name") {
                        node.name = name;
                    }
                    if let Some(d) = text(props, "node.description") {
                        node.description = Some(d);
                    }
                    if let Some(n) = text(props, "node.nick") {
                        node.nick = Some(n);
                    }
                }
                // props that name another class (video, midi) or none: not ours
                None => {
                    if props.get("media.class").is_some() {
                        self.nodes.remove(&id);
                    }
                }
            }
        }
        if let (Some(node), Some((volumes, muted))) = (self.nodes.get_mut(&id), props_param(info)) {
            node.channel_volumes = volumes;
            node.muted = muted;
        }
    }

    fn default_sink_id(&self) -> Option<u32> {
        let name = self.default_sink.as_deref()?;
        self.nodes
            .iter()
            .find(|(_, n)| n.class == Class::Sink && n.name == name)
            .map(|(id, _)| *id)
    }

    fn intent(
        &self,
        verb: &str,
        id: Option<u32>,
        volume: Option<f64>,
        muted: Option<bool>,
    ) -> Effect {
        let refuse = |reason: &str, message: String| Effect::Refuse {
            verb: verb.into(),
            reason: reason.into(),
            message,
        };
        if !matches!(verb, "set_volume" | "set_mute" | "set_default") {
            return refuse("unsupported", format!("audio: unknown verb `{verb}`"));
        }
        if !self.available {
            let why = self.why.clone().unwrap_or_default();
            return refuse("unavailable", format!("{verb}: {why}"));
        }
        // what the call will be, once its target is known
        let call: Box<dyn Fn(u32) -> Call> = match (verb, volume.filter(|v| v.is_finite()), muted) {
            ("set_volume", Some(v), _) => {
                let volume = v.clamp(0.0, MAX_VOLUME);
                Box::new(move |id| Call::SetVolume { id, volume })
            }
            ("set_volume", None, _) => {
                return refuse("bad_arg", format!("{verb}: needs a number `volume`"))
            }
            ("set_mute", _, Some(muted)) => Box::new(move |id| Call::SetMute { id, muted }),
            ("set_mute", _, None) => {
                return refuse("bad_arg", format!("{verb}: needs `muted`, true or false"))
            }
            _ if id.is_none() => {
                return refuse(
                    "bad_arg",
                    format!("{verb}: needs the `id` of a sink or source"),
                )
            }
            _ => Box::new(|id| Call::SetDefault { id }),
        };
        let target = match id {
            Some(i) if self.nodes.contains_key(&i) => i,
            Some(i) => return refuse("not_found", format!("{verb}: no sink or source {i}")),
            None => match self.default_sink_id() {
                Some(i) => i,
                None => return refuse("not_found", format!("{verb}: no default sink")),
            },
        };
        Effect::Call(call(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIXTURE: &str = include_str!("../tests/fixtures/pw_dump.json");

    fn dumped() -> AudioVm {
        let mut vm = AudioVm::new(Config::default());
        vm.reduce(Event::Dump(serde_json::from_str(FIXTURE).unwrap()));
        vm
    }

    fn node(id: u32, class: &str, name: &str, volumes: &[f64], mute: bool) -> Value {
        json!({"id": id, "type": NODE, "info": {
            "props": {"media.class": class, "node.name": name},
            "params": {"Props": [{"mute": mute, "channelVolumes": volumes}]}}})
    }

    fn one_sink(volumes: &[f64]) -> f64 {
        let mut vm = AudioVm::new(Config::default());
        vm.reduce(Event::Dump(json!([node(
            1,
            "Audio/Sink",
            "s",
            volumes,
            false
        )])));
        vm.state().sinks[0].volume
    }

    #[test]
    fn pw_dump_fixture_lists_sinks_sources_and_marks_the_default_sink() {
        let s = dumped().state();
        assert!(s.available && s.why.is_none());
        let ids: Vec<u32> = s.sinks.iter().chain(&s.sources).map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec![52, 53, 54],
            "the video node and the client are not listed"
        );
        assert_eq!(s.sinks.iter().filter(|d| d.default).count(), 1);
        let d = &s.sinks[0];
        assert_eq!(
            (
                d.name.as_str(),
                d.description.as_str(),
                d.volume,
                d.muted,
                d.default
            ),
            (
                "alsa_output.pci-0000_00_1b.0.analog-stereo",
                "Built-in Audio Analog Stereo",
                0.40,
                false,
                true
            )
        );
        assert_eq!(
            s.sinks[1].description,
            "Built-in Audio Digital Stereo (HDMI)"
        );
        assert!(!s.sinks[1].default && s.sinks[1].muted);
        // no description and no nick: the name
        let src = &s.sources[0];
        assert_eq!(src.description, "alsa_input.pci-0000_00_1b.0.analog-stereo");
        assert!(src.default);
        assert_eq!(src.volume, 0.5);
    }

    #[test]
    fn volume_is_the_cube_root_of_the_mean_channel_volume_rounded_to_two_places() {
        assert_eq!(one_sink(&[0.027, 0.027]), 0.30);
        assert_eq!(one_sink(&[0.064, 0.064]), 0.40);
        assert_eq!(one_sink(&[0.0, 0.216]), 0.48);
        assert_eq!(one_sink(&[]), 0.0);
    }

    #[test]
    fn mute_comes_from_props_and_a_missing_props_is_unmuted() {
        let mut vm = AudioVm::new(Config::default());
        vm.reduce(Event::Dump(json!([
            node(1, "Audio/Sink", "a", &[1.0], true),
            {"id": 2, "type": NODE, "info": {"props": {"media.class": "Audio/Sink", "node.name": "b"}}}
        ])));
        let s = vm.state();
        assert!(s.sinks[0].muted);
        assert!(!s.sinks[1].muted);
        assert_eq!(s.sinks[1].volume, 0.0);
    }

    #[test]
    fn a_delta_updates_volume_and_mute_of_a_known_node_and_keeps_the_rest() {
        let mut vm = dumped();
        vm.reduce(Event::Delta(json!([{"id": 52, "type": NODE, "info": {
            "params": {"Props": [{"mute": true, "channelVolumes": [0.027, 0.027]}]}}}])));
        let d = vm.state().sinks[0].clone();
        assert_eq!((d.volume, d.muted), (0.30, true));
        assert_eq!(d.name, "alsa_output.pci-0000_00_1b.0.analog-stereo");
        assert_eq!(d.description, "Built-in Audio Analog Stereo");
        assert!(d.default);
        // props without params keep the volume
        vm.reduce(Event::Delta(json!([{"id": 52, "type": NODE, "info": {
            "props": {"media.class": "Audio/Sink", "node.name": "alsa_output.pci-0000_00_1b.0.analog-stereo"}}}])));
        let d = vm.state().sinks[0].clone();
        assert_eq!((d.volume, d.muted), (0.30, true));
        assert_eq!(d.description, "Built-in Audio Analog Stereo");
    }

    #[test]
    fn a_delta_with_info_null_removes_the_node() {
        let mut vm = dumped();
        vm.reduce(Event::Delta(json!([{"id": 53, "info": null}])));
        let ids: Vec<u32> = vm.state().sinks.iter().map(|d| d.id).collect();
        assert_eq!(ids, vec![52]);
    }

    #[test]
    fn a_delta_to_the_default_metadata_moves_the_default_flag() {
        let mut vm = dumped();
        vm.reduce(Event::Delta(
            json!([{"id": 30, "type": METADATA, "metadata": [
            {"subject": 0, "key": "default.audio.sink", "type": "Spa:String:JSON",
             "value": "{\"name\":\"alsa_output.pci-0000_00_1b.0.hdmi-stereo\"}"}]}]),
        ));
        let s = vm.state();
        assert_eq!(
            s.sinks.iter().map(|d| d.default).collect::<Vec<_>>(),
            vec![false, true]
        );
        assert!(s.sources[0].default, "the source's default is untouched");
    }

    #[test]
    fn a_delta_for_a_new_sink_adds_it_in_id_order() {
        let mut vm = dumped();
        vm.reduce(Event::Delta(json!([node(
            50,
            "Audio/Sink",
            "usb",
            &[1.0],
            false
        )])));
        let ids: Vec<u32> = vm.state().sinks.iter().map(|d| d.id).collect();
        assert_eq!(ids, vec![50, 52, 53]);
    }

    #[test]
    fn start_and_changed_ask_for_a_read_and_an_available_tick_does_nothing() {
        let mut vm = AudioVm::new(Config::default());
        assert_eq!(vm.reduce(Event::Start), vec![Effect::Read]);
        assert_eq!(vm.reduce(Event::Changed), vec![Effect::Read]);
        let mut vm = dumped();
        assert!(vm.reduce(Event::Tick { now_ms: 1_000_000 }).is_empty());
    }

    #[test]
    fn a_tick_while_unavailable_asks_for_a_read_until_a_dump_arrives() {
        let mut fresh = AudioVm::new(Config::default());
        assert_eq!(fresh.reduce(Event::Tick { now_ms: 1 }), vec![Effect::Read]);
        let mut vm = dumped();
        vm.reduce(Event::Unavailable {
            why: "pw-dump took longer than 5 s".into(),
        });
        for now_ms in [5_000, 10_000] {
            assert_eq!(vm.reduce(Event::Tick { now_ms }), vec![Effect::Read]);
        }
        vm.reduce(Event::Dump(serde_json::from_str(FIXTURE).unwrap()));
        assert!(vm.state().available);
        assert!(vm.reduce(Event::Tick { now_ms: 15_000 }).is_empty());
    }

    #[test]
    fn unavailable_event_sets_available_false_with_why_and_a_dump_restores_it() {
        let mut vm = dumped();
        vm.reduce(Event::Unavailable {
            why: "wpctl is not installed".into(),
        });
        let s = vm.state();
        assert!(!s.available);
        assert_eq!(s.why.as_deref(), Some("wpctl is not installed"));
        vm.reduce(Event::Dump(serde_json::from_str(FIXTURE).unwrap()));
        let s = vm.state();
        assert!(s.available && s.why.is_none());
    }

    fn intent(
        vm: &mut AudioVm,
        verb: &str,
        id: Option<u32>,
        volume: Option<f64>,
        muted: Option<bool>,
    ) -> Effect {
        vm.reduce(Event::Intent {
            verb: verb.into(),
            id,
            volume,
            muted,
        })
        .remove(0)
    }

    fn reason(e: &Effect) -> &str {
        match e {
            Effect::Refuse { reason, .. } => reason,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    #[test]
    fn set_volume_clamps_to_one_and_a_half_and_defaults_to_the_default_sink() {
        let mut vm = dumped();
        assert_eq!(
            intent(&mut vm, "set_volume", None, Some(9.0), None),
            Effect::Call(Call::SetVolume {
                id: 52,
                volume: 1.5
            })
        );
        assert_eq!(
            intent(&mut vm, "set_volume", Some(53), Some(-2.0), None),
            Effect::Call(Call::SetVolume {
                id: 53,
                volume: 0.0
            })
        );
        assert_eq!(
            intent(&mut vm, "set_volume", None, Some(0.3), None),
            Effect::Call(Call::SetVolume {
                id: 52,
                volume: 0.3
            })
        );
        assert_eq!(
            intent(&mut vm, "set_mute", None, None, Some(true)),
            Effect::Call(Call::SetMute {
                id: 52,
                muted: true
            })
        );
        assert_eq!(
            intent(&mut vm, "set_default", Some(54), None, None),
            Effect::Call(Call::SetDefault { id: 54 })
        );
    }

    #[test]
    fn set_volume_without_a_number_is_bad_arg() {
        let mut vm = dumped();
        assert_eq!(
            reason(&intent(&mut vm, "set_volume", None, None, None)),
            "bad_arg"
        );
        assert_eq!(
            reason(&intent(&mut vm, "set_volume", None, Some(f64::NAN), None)),
            "bad_arg"
        );
    }

    #[test]
    fn set_mute_without_muted_is_bad_arg() {
        let mut vm = dumped();
        assert_eq!(
            reason(&intent(&mut vm, "set_mute", Some(52), None, None)),
            "bad_arg"
        );
    }

    #[test]
    fn an_unknown_id_is_not_found_and_set_default_needs_an_id() {
        let mut vm = dumped();
        assert_eq!(
            reason(&intent(&mut vm, "set_volume", Some(99), Some(0.5), None)),
            "not_found"
        );
        // the video node is no sink
        assert_eq!(
            reason(&intent(&mut vm, "set_mute", Some(60), None, Some(true))),
            "not_found"
        );
        assert_eq!(
            reason(&intent(&mut vm, "set_default", None, None, None)),
            "bad_arg"
        );
        assert_eq!(
            reason(&intent(&mut vm, "louder", None, None, None)),
            "unsupported"
        );
        // no default sink: a verb without an id has nothing to act on
        let mut bare = AudioVm::new(Config::default());
        bare.reduce(Event::Dump(json!([node(
            1,
            "Audio/Sink",
            "s",
            &[1.0],
            false
        )])));
        assert_eq!(
            reason(&intent(&mut bare, "set_volume", None, Some(0.5), None)),
            "not_found"
        );
    }

    #[test]
    fn intents_are_unavailable_before_the_first_read() {
        let mut vm = AudioVm::new(Config::default());
        let e = intent(&mut vm, "set_volume", None, Some(0.5), None);
        assert_eq!(reason(&e), "unavailable");
        assert!(
            matches!(&e, Effect::Refuse { message, .. } if message == "set_volume: not read yet")
        );
    }

    #[test]
    fn the_state_serialises_as_the_documented_json() {
        let mut vm = AudioVm::new(Config::default());
        vm.reduce(Event::Dump(json!([
            node(52, "Audio/Sink", "out", &[0.064, 0.064], false),
            node(54, "Audio/Source", "in", &[1.0], true),
            {"id": 30, "type": METADATA, "props": {"metadata.name": "default"}, "metadata": [
                {"subject": 0, "key": "default.audio.sink", "value": {"name": "out"}}]}
        ])));
        assert_eq!(
            serde_json::to_value(vm.state()).unwrap(),
            json!({"available": true,
                   "sinks": [{"id": 52, "name": "out", "description": "out", "volume": 0.4, "muted": false, "default": true}],
                   "sources": [{"id": 54, "name": "in", "description": "in", "volume": 1.0, "muted": true, "default": false}]})
        );
    }

    #[test]
    fn the_state_json_is_available_false_with_a_why_before_the_first_read() {
        let j = serde_json::to_value(AudioVm::new(Config::default()).state()).unwrap();
        assert_eq!(
            j,
            json!({"available": false, "why": "not read yet", "sinks": [], "sources": []})
        );
    }
}
