//! THE NETWORK AGENT: NetworkManager's devices, Wi-Fi access points and saved
//! connections. The sys layer flattens NetworkManager's D-Bus property maps
//! into a [`Read`] and runs the [`Call`]s; this module names the codes, groups
//! the access points, classifies their security, builds the settings tree of a
//! new connection and decides every refusal. The Wi-Fi password lives only in
//! an intent and in the settings tree of one call: never in the state, never in
//! a debug string.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// How often the whole tree is read again without a signal, in milliseconds.
pub const REREAD_MS: u64 = 30_000;

/// `NM80211ApFlags`: the AP needs encryption.
pub const AP_PRIVACY: u32 = 0x1;
/// `NM80211ApSecurityFlags`: pre-shared key.
pub const KEY_MGMT_PSK: u32 = 0x100;
/// `NM80211ApSecurityFlags`: 802.1X.
pub const KEY_MGMT_802_1X: u32 = 0x200;
/// `NM80211ApSecurityFlags`: SAE (WPA3 personal).
pub const KEY_MGMT_SAE: u32 = 0x400;

/// A password that never prints: no `Serialize`, a redacted `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// One value of a NetworkManager settings map (`a{sa{sv}}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    U32(u32),
    StrList(Vec<String>),
}

/// A settings tree: section -> key -> value. Its `Debug` hides every `psk`.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Settings(pub BTreeMap<String, BTreeMap<String, Setting>>);

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sections = f.debug_map();
        for (name, keys) in &self.0 {
            let shown: BTreeMap<&String, String> = keys
                .iter()
                .map(|(k, v)| {
                    let v = if k == "psk" {
                        "<redacted>".to_string()
                    } else {
                        format!("{v:?}")
                    };
                    (k, v)
                })
                .collect();
            sections.entry(name, &shown);
        }
        sections.finish()
    }
}

/// One access point as NetworkManager has it (`ssid` is raw bytes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApRead {
    pub path: String,
    pub ssid: Vec<u8>,
    pub strength: u8,
    pub flags: u32,
    pub wpa_flags: u32,
    pub rsn_flags: u32,
    pub frequency: u32,
    pub active: bool,
}

/// One device with its addresses and, when Wi-Fi, its access points.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRead {
    pub path: String,
    pub iface: String,
    pub device_type: u32,
    pub state: u32,
    /// The active connection's id; empty when there is none.
    pub connection: String,
    pub ip4: Vec<String>,
    #[serde(default)]
    pub aps: Vec<ApRead>,
}

/// One saved connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownRead {
    pub uuid: String,
    pub id: String,
    /// `connection.type`: `802-11-wireless`, `802-3-ethernet`, …
    pub kind: String,
    pub autoconnect: bool,
    /// A Wi-Fi connection's raw SSID.
    #[serde(default)]
    pub ssid: Option<Vec<u8>>,
}

/// One read of NetworkManager, flattened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Read {
    pub state: u32,
    pub connectivity: u32,
    pub wireless_enabled: bool,
    pub wireless_hw_enabled: bool,
    pub devices: Vec<DeviceRead>,
    pub known: Vec<KnownRead>,
}

/// What a verb brings; the password is a [`Secret`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub ssid: Option<String>,
    pub password: Option<Secret>,
    pub uuid: Option<String>,
    pub iface: Option<String>,
    pub enabled: Option<bool>,
}

/// A NetworkManager call, as plain data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// `RequestScan` on every Wi-Fi device.
    Scan,
    /// `ActivateConnection(connection with this uuid, the device named, "/")`.
    Activate { uuid: String, iface: String },
    /// `AddAndActivateConnection(settings, the device named, the AP's path)`.
    ConnectNew {
        iface: String,
        ap: String,
        settings: Settings,
    },
    /// `DeactivateConnection` of the device's active connection.
    Deactivate { iface: String },
    /// `Delete()` on the saved connection with this uuid.
    Forget { uuid: String },
    /// Set `WirelessEnabled`.
    SetWireless { enabled: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// An answer to [`Effect::Read`].
    Read(Read),
    /// Something changed on the bus (or a call went through): read again.
    Changed,
    /// A verb: `scan`, `connect`, `activate`, `deactivate`, `forget` or `set_wireless`.
    Intent { verb: String, args: Args },
    /// NetworkManager is not there (or a read failed).
    Unavailable { why: String },
    /// Time, for the periodic re-read.
    Tick { now_ms: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug)]
pub struct Config {
    /// The user the new connections belong to (`connection.permissions`).
    pub user: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            user: "fantastic".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Wireless {
    pub enabled: bool,
    pub hardware_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Device {
    pub iface: String,
    pub kind: String,
    pub state: String,
    pub connection: String,
    pub ip4: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AccessPoint {
    pub ssid: String,
    pub strength: u8,
    pub security: String,
    pub frequency_mhz: u32,
    pub active: bool,
    pub known: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Known {
    pub uuid: String,
    pub id: String,
    pub kind: String,
    pub autoconnect: bool,
}

/// What the view draws; its JSON is the agent's `state` reply.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct State {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub state: String,
    pub connectivity: String,
    pub wireless: Wireless,
    pub devices: Vec<Device>,
    pub access_points: Vec<AccessPoint>,
    pub known: Vec<Known>,
    pub busy: Option<String>,
}

/// `NMState`.
pub fn nm_state_name(code: u32) -> &'static str {
    match code {
        10 => "asleep",
        20 => "disconnected",
        30 => "disconnecting",
        40 => "connecting",
        50 => "connected_local",
        60 => "connected_site",
        70 => "connected_global",
        _ => "unknown",
    }
}

/// `NMConnectivityState`.
pub fn connectivity_name(code: u32) -> &'static str {
    match code {
        1 => "none",
        2 => "portal",
        3 => "limited",
        4 => "full",
        _ => "unknown",
    }
}

/// `NMDeviceState`.
pub fn device_state_name(code: u32) -> &'static str {
    match code {
        10 => "unmanaged",
        20 => "unavailable",
        30 => "disconnected",
        40 => "prepare",
        50 => "config",
        60 => "need_auth",
        70 => "ip_config",
        80 => "ip_check",
        90 => "secondaries",
        100 => "activated",
        110 => "deactivating",
        120 => "failed",
        _ => "unknown",
    }
}

/// `NMDeviceType`: 1 ethernet, 2 wifi, else other.
pub fn device_kind_name(code: u32) -> &'static str {
    match code {
        1 => "ethernet",
        2 => "wifi",
        _ => "other",
    }
}

/// A saved connection's `connection.type` as a kind.
pub fn connection_kind_name(kind: &str) -> &'static str {
    match kind {
        "802-11-wireless" => "wifi",
        "802-3-ethernet" => "ethernet",
        _ => "other",
    }
}

/// The security of an access point, from its `Flags`, `WpaFlags` and `RsnFlags`.
pub fn security_of(flags: u32, wpa_flags: u32, rsn_flags: u32) -> &'static str {
    let keys = wpa_flags | rsn_flags;
    if keys & KEY_MGMT_SAE != 0 {
        "sae"
    } else if keys & KEY_MGMT_802_1X != 0 {
        "enterprise"
    } else if keys & KEY_MGMT_PSK != 0 {
        "wpa-psk"
    } else if flags & AP_PRIVACY != 0 && keys == 0 {
        "wep"
    } else {
        "open"
    }
}

/// An SSID as shown: its text when that is UTF-8, else `0x<hex>`.
pub fn ssid_shown(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
            format!("0x{hex}")
        }
    }
}

/// An access point after grouping by SSID.
struct Seen {
    shown: String,
    raw: Vec<u8>,
    strength: u8,
    flags: u32,
    wpa_flags: u32,
    rsn_flags: u32,
    frequency: u32,
    active: bool,
    iface: String,
    path: String,
}

pub struct NetworkVm {
    cfg: Config,
    available: bool,
    why: Option<String>,
    read: Option<Read>,
    last_read_ms: Option<u64>,
}

impl NetworkVm {
    pub fn new(cfg: Config) -> Self {
        NetworkVm {
            cfg,
            available: false,
            why: Some("not read yet".into()),
            read: None,
            last_read_ms: None,
        }
    }

    pub fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        match ev {
            Event::Read(r) => {
                self.read = Some(r);
                self.available = true;
                self.why = None;
                vec![]
            }
            Event::Changed => vec![Effect::Read],
            Event::Unavailable { why } => {
                self.available = false;
                self.why = Some(why);
                self.read = None;
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
            Event::Intent { verb, args } => vec![self.intent(&verb, &args)],
        }
    }

    pub fn state(&self) -> State {
        let Some(r) = &self.read else {
            return State {
                available: self.available,
                why: self.why.clone(),
                state: nm_state_name(0).into(),
                connectivity: connectivity_name(0).into(),
                wireless: Wireless {
                    enabled: false,
                    hardware_enabled: false,
                },
                devices: vec![],
                access_points: vec![],
                known: vec![],
                busy: None,
            };
        };
        let access_points = self
            .seen()
            .into_iter()
            .map(|s| AccessPoint {
                known: self.known_for(&s.raw).is_some(),
                security: security_of(s.flags, s.wpa_flags, s.rsn_flags).into(),
                ssid: s.shown,
                strength: s.strength,
                frequency_mhz: s.frequency,
                active: s.active,
            })
            .collect();
        State {
            available: self.available,
            why: self.why.clone(),
            state: nm_state_name(r.state).into(),
            connectivity: connectivity_name(r.connectivity).into(),
            wireless: Wireless {
                enabled: r.wireless_enabled,
                hardware_enabled: r.wireless_hw_enabled,
            },
            devices: r
                .devices
                .iter()
                .filter(|d| d.iface != "lo")
                .map(|d| Device {
                    iface: d.iface.clone(),
                    kind: device_kind_name(d.device_type).into(),
                    state: device_state_name(d.state).into(),
                    connection: d.connection.clone(),
                    ip4: d.ip4.clone(),
                })
                .collect(),
            access_points,
            known: r
                .known
                .iter()
                .map(|k| Known {
                    uuid: k.uuid.clone(),
                    id: k.id.clone(),
                    kind: connection_kind_name(&k.kind).into(),
                    autoconnect: k.autoconnect,
                })
                .collect(),
            busy: None,
        }
    }

    /// The access points of every Wi-Fi device, one per SSID (the strongest),
    /// strongest first. An AP with no SSID (hidden) is not listed.
    fn seen(&self) -> Vec<Seen> {
        let Some(r) = &self.read else { return vec![] };
        let mut by_ssid: Vec<Seen> = Vec::new();
        for dev in r.devices.iter().filter(|d| d.device_type == 2) {
            for ap in dev.aps.iter().filter(|a| !a.ssid.is_empty()) {
                match by_ssid.iter_mut().find(|s| s.raw == ap.ssid) {
                    Some(s) => {
                        s.active |= ap.active;
                        if ap.strength > s.strength {
                            s.strength = ap.strength;
                            s.flags = ap.flags;
                            s.wpa_flags = ap.wpa_flags;
                            s.rsn_flags = ap.rsn_flags;
                            s.frequency = ap.frequency;
                            s.iface = dev.iface.clone();
                            s.path = ap.path.clone();
                        }
                    }
                    None => by_ssid.push(Seen {
                        shown: ssid_shown(&ap.ssid),
                        raw: ap.ssid.clone(),
                        strength: ap.strength,
                        flags: ap.flags,
                        wpa_flags: ap.wpa_flags,
                        rsn_flags: ap.rsn_flags,
                        frequency: ap.frequency,
                        active: ap.active,
                        iface: dev.iface.clone(),
                        path: ap.path.clone(),
                    }),
                }
            }
        }
        by_ssid.sort_by(|a, b| {
            b.strength
                .cmp(&a.strength)
                .then_with(|| a.shown.cmp(&b.shown))
        });
        by_ssid
    }

    /// The saved Wi-Fi connection of this SSID.
    fn known_for(&self, raw: &[u8]) -> Option<&KnownRead> {
        self.read
            .as_ref()?
            .known
            .iter()
            .find(|k| connection_kind_name(&k.kind) == "wifi" && k.ssid.as_deref() == Some(raw))
    }

    fn wifi_iface(&self) -> Option<&str> {
        self.read
            .as_ref()?
            .devices
            .iter()
            .find(|d| d.device_type == 2)
            .map(|d| d.iface.as_str())
    }

    fn intent(&self, verb: &str, a: &Args) -> Effect {
        let refuse = |reason: &str, message: String| Effect::Refuse {
            verb: verb.into(),
            reason: reason.into(),
            message,
        };
        if !matches!(
            verb,
            "scan" | "connect" | "activate" | "deactivate" | "forget" | "set_wireless"
        ) {
            return refuse("unsupported", format!("network: unknown verb `{verb}`"));
        }
        let (true, Some(r)) = (self.available, &self.read) else {
            let why = self.why.clone().unwrap_or_default();
            return refuse("unavailable", format!("{verb}: {why}"));
        };
        match verb {
            "scan" => {
                if !r.wireless_enabled {
                    return refuse("unavailable", "scan: wireless is off".into());
                }
                if self.wifi_iface().is_none() {
                    return refuse("not_found", "scan: there is no Wi-Fi device".into());
                }
                Effect::Call(Call::Scan)
            }
            "connect" => self.connect(a),
            "activate" => {
                let Some(uuid) = a.uuid.as_deref() else {
                    return refuse(
                        "bad_arg",
                        "activate: needs the `uuid` of a saved connection".into(),
                    );
                };
                let Some(k) = r.known.iter().find(|k| k.uuid == uuid) else {
                    return refuse("not_found", format!("activate: no saved connection {uuid}"));
                };
                let kind = connection_kind_name(&k.kind);
                let device = r
                    .devices
                    .iter()
                    .find(|d| kind != "other" && device_kind_name(d.device_type) == kind);
                match device {
                    Some(d) => Effect::Call(Call::Activate {
                        uuid: uuid.into(),
                        iface: d.iface.clone(),
                    }),
                    None => refuse(
                        "not_found",
                        format!("activate: no {kind} device for `{}`", k.id),
                    ),
                }
            }
            "deactivate" => {
                let Some(iface) = a.iface.as_deref() else {
                    return refuse(
                        "bad_arg",
                        "deactivate: needs the `iface` of a device".into(),
                    );
                };
                match r.devices.iter().find(|d| d.iface == iface) {
                    None => refuse("not_found", format!("deactivate: no device {iface}")),
                    Some(d) if d.connection.is_empty() => refuse(
                        "not_found",
                        format!("deactivate: {iface} has no active connection"),
                    ),
                    Some(_) => Effect::Call(Call::Deactivate {
                        iface: iface.into(),
                    }),
                }
            }
            "forget" => {
                let Some(uuid) = a.uuid.as_deref() else {
                    return refuse(
                        "bad_arg",
                        "forget: needs the `uuid` of a saved connection".into(),
                    );
                };
                if r.known.iter().any(|k| k.uuid == uuid) {
                    Effect::Call(Call::Forget { uuid: uuid.into() })
                } else {
                    refuse("not_found", format!("forget: no saved connection {uuid}"))
                }
            }
            _ => {
                let Some(enabled) = a.enabled else {
                    return refuse(
                        "bad_arg",
                        "set_wireless: needs `enabled`, true or false".into(),
                    );
                };
                if enabled && !r.wireless_hw_enabled {
                    return refuse(
                        "unsupported",
                        "set_wireless: the wireless hardware switch is off".into(),
                    );
                }
                Effect::Call(Call::SetWireless { enabled })
            }
        }
    }

    fn connect(&self, a: &Args) -> Effect {
        let refuse = |reason: &str, message: String| Effect::Refuse {
            verb: "connect".into(),
            reason: reason.into(),
            message,
        };
        let Some(r) = &self.read else {
            return refuse("unavailable", "connect: not read yet".into());
        };
        let Some(ssid) = a.ssid.as_deref().filter(|s| !s.is_empty()) else {
            return refuse("bad_arg", "connect: needs the `ssid` of a network".into());
        };
        if !r.wireless_enabled {
            return refuse("unavailable", "connect: wireless is off".into());
        }
        let Some(ap) = self.seen().into_iter().find(|s| s.shown == ssid) else {
            return refuse(
                "not_found",
                format!("connect: no network `{ssid}` in range"),
            );
        };
        // a saved connection needs no password, whatever its security
        if let Some(k) = self.known_for(&ap.raw) {
            return Effect::Call(Call::Activate {
                uuid: k.uuid.clone(),
                iface: ap.iface,
            });
        }
        let security = security_of(ap.flags, ap.wpa_flags, ap.rsn_flags);
        let mut wireless = BTreeMap::new();
        wireless.insert("ssid".to_string(), Setting::Bytes(ap.raw.clone()));
        wireless.insert("mode".to_string(), Setting::Str("infrastructure".into()));
        let mut tree = BTreeMap::new();
        match security {
            "enterprise" | "wep" => {
                return refuse(
                    "unsupported",
                    format!("connect: `{ssid}` uses {security}, which is not supported"),
                )
            }
            "open" => {}
            _ => {
                let Some(pw) = a.password.as_ref().filter(|p| !p.expose().is_empty()) else {
                    return refuse("bad_arg", format!("connect: `{ssid}` needs a `password`"));
                };
                if !password_fits(pw.expose()) {
                    return refuse(
                        "bad_arg",
                        "connect: a WPA password is 8 to 63 characters, or 64 hex digits".into(),
                    );
                }
                let mut sec = BTreeMap::new();
                sec.insert("key-mgmt".to_string(), Setting::Str(security.into()));
                sec.insert("psk".to_string(), Setting::Str(pw.expose().into()));
                tree.insert("802-11-wireless-security".to_string(), sec);
            }
        }
        let mut connection = BTreeMap::new();
        connection.insert("type".to_string(), Setting::Str("802-11-wireless".into()));
        connection.insert("id".to_string(), Setting::Str(ssid.into()));
        connection.insert(
            "permissions".to_string(),
            Setting::StrList(vec![format!("user:{}", self.cfg.user)]),
        );
        tree.insert("connection".to_string(), connection);
        tree.insert("802-11-wireless".to_string(), wireless);
        Effect::Call(Call::ConnectNew {
            iface: ap.iface,
            ap: ap.path,
            settings: Settings(tree),
        })
    }
}

/// A WPA passphrase: 8 to 63 characters, or a 64-digit hex key.
fn password_fits(p: &str) -> bool {
    let n = p.chars().count();
    (8..=63).contains(&n) || (n == 64 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/network_aps.json");

    fn ap(path: &str, ssid: &[u8], strength: u8, flags: u32, wpa: u32, rsn: u32) -> ApRead {
        ApRead {
            path: path.into(),
            ssid: ssid.to_vec(),
            strength,
            flags,
            wpa_flags: wpa,
            rsn_flags: rsn,
            frequency: 2437,
            active: false,
        }
    }

    fn device(iface: &str, device_type: u32, state: u32, aps: Vec<ApRead>) -> DeviceRead {
        DeviceRead {
            path: format!("/dev/{iface}"),
            iface: iface.into(),
            device_type,
            state,
            connection: String::new(),
            ip4: vec![],
            aps,
        }
    }

    fn read(devices: Vec<DeviceRead>, known: Vec<KnownRead>) -> Read {
        Read {
            state: 70,
            connectivity: 4,
            wireless_enabled: true,
            wireless_hw_enabled: true,
            devices,
            known,
        }
    }

    fn vm(r: Read) -> NetworkVm {
        let mut vm = NetworkVm::new(Config {
            user: "fantastic".into(),
        });
        vm.reduce(Event::Read(r));
        vm
    }

    fn wifi(aps: Vec<ApRead>) -> NetworkVm {
        vm(read(vec![device("wlan0", 2, 30, aps)], vec![]))
    }

    fn intent(vm: &mut NetworkVm, verb: &str, args: Args) -> Effect {
        vm.reduce(Event::Intent {
            verb: verb.into(),
            args,
        })
        .remove(0)
    }

    fn reason(e: &Effect) -> &str {
        match e {
            Effect::Refuse { reason, .. } => reason,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    fn message(e: &Effect) -> &str {
        match e {
            Effect::Refuse { message, .. } => message,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    fn ssid_arg(s: &str) -> Args {
        Args {
            ssid: Some(s.into()),
            ..Args::default()
        }
    }

    fn with_password(s: &str, p: &str) -> Args {
        Args {
            password: Some(Secret::new(p)),
            ..ssid_arg(s)
        }
    }

    #[test]
    fn nm_state_connectivity_and_device_state_names_map() {
        let states = [
            (0, "unknown"),
            (10, "asleep"),
            (20, "disconnected"),
            (30, "disconnecting"),
            (40, "connecting"),
            (50, "connected_local"),
            (60, "connected_site"),
            (70, "connected_global"),
            (15, "unknown"),
            (999, "unknown"),
        ];
        for (code, name) in states {
            assert_eq!(nm_state_name(code), name, "NMState {code}");
        }
        let connectivity = [
            (0, "unknown"),
            (1, "none"),
            (2, "portal"),
            (3, "limited"),
            (4, "full"),
            (5, "unknown"),
        ];
        for (code, name) in connectivity {
            assert_eq!(connectivity_name(code), name, "NMConnectivityState {code}");
        }
        let devices = [
            (0, "unknown"),
            (10, "unmanaged"),
            (20, "unavailable"),
            (30, "disconnected"),
            (40, "prepare"),
            (50, "config"),
            (60, "need_auth"),
            (70, "ip_config"),
            (80, "ip_check"),
            (90, "secondaries"),
            (100, "activated"),
            (110, "deactivating"),
            (120, "failed"),
            (121, "unknown"),
        ];
        for (code, name) in devices {
            assert_eq!(device_state_name(code), name, "NMDeviceState {code}");
        }
        let kinds = [
            (1, "ethernet"),
            (2, "wifi"),
            (0, "other"),
            (14, "other"),
            (32, "other"),
        ];
        for (code, name) in kinds {
            assert_eq!(device_kind_name(code), name, "NMDeviceType {code}");
        }
    }

    #[test]
    fn access_points_dedup_by_ssid_keep_strongest_and_sort_by_strength() {
        let vm = wifi(vec![
            ap("/a1", b"home", 40, 1, 0, 0x108),
            ap("/a2", b"cafe", 80, 0, 0, 0),
            ap("/a3", b"home", 72, 1, 0, 0x108),
            ap("/a4", b"zeta", 40, 0, 0, 0),
            ap("/a5", b"home", 10, 1, 0, 0x108),
        ]);
        let aps = vm.state().access_points;
        let order: Vec<(&str, u8)> = aps.iter().map(|a| (a.ssid.as_str(), a.strength)).collect();
        assert_eq!(order, vec![("cafe", 80), ("home", 72), ("zeta", 40)]);
    }

    #[test]
    fn dedup_spans_wifi_devices_and_remembers_the_active_one() {
        let mut weak = ap("/w", b"home", 30, 1, 0, 0x108);
        weak.active = true;
        let vm = vm(read(
            vec![
                device("wlan0", 2, 100, vec![weak]),
                device("wlan1", 2, 30, vec![ap("/s", b"home", 90, 1, 0, 0x108)]),
            ],
            vec![],
        ));
        let aps = vm.state().access_points;
        assert_eq!(aps.len(), 1);
        assert_eq!((aps[0].strength, aps[0].active), (90, true));
    }

    #[test]
    fn non_utf8_ssid_is_shown_as_0x_hex_and_raw_bytes_kept() {
        let raw = [0xff, 0xfe, 0x41];
        let mut vm = wifi(vec![
            ap("/x", &raw, 55, 0, 0, 0),
            ap("/y", b"", 90, 0, 0, 0),
        ]);
        let aps = vm.state().access_points;
        assert_eq!(aps.len(), 1, "a hidden network (empty SSID) is not listed");
        assert_eq!(aps[0].ssid, "0xfffe41");
        assert_eq!(ssid_shown("café".as_bytes()), "café");
        // connecting by the shown name builds the tree with the raw bytes
        let Effect::Call(Call::ConnectNew { settings, .. }) =
            intent(&mut vm, "connect", ssid_arg("0xfffe41"))
        else {
            panic!("an open network connects")
        };
        assert_eq!(
            settings.0["802-11-wireless"]["ssid"],
            Setting::Bytes(raw.to_vec())
        );
    }

    #[test]
    fn security_is_classified_from_flags_wpa_and_rsn() {
        let table = [
            // (flags, wpa, rsn, security)
            (0, 0, 0, "open"),
            (1, 0, 0, "wep"),
            (1, 0, 0x8 | 0x80, "open"),
            (1, 0x108, 0, "wpa-psk"),
            (1, 0, 0x188, "wpa-psk"),
            (1, 0, KEY_MGMT_SAE | 0x8, "sae"),
            (1, 0, KEY_MGMT_802_1X | 0x8, "enterprise"),
            // precedence: sae over psk, enterprise over psk, sae over enterprise
            (1, 0, KEY_MGMT_PSK | KEY_MGMT_SAE, "sae"),
            (1, 0, KEY_MGMT_PSK | KEY_MGMT_802_1X, "enterprise"),
            (1, KEY_MGMT_802_1X, KEY_MGMT_SAE, "sae"),
            (1, KEY_MGMT_PSK, KEY_MGMT_PSK, "wpa-psk"),
        ];
        for (flags, wpa, rsn, want) in table {
            assert_eq!(
                security_of(flags, wpa, rsn),
                want,
                "flags {flags:#x} wpa {wpa:#x} rsn {rsn:#x}"
            );
        }
    }

    #[test]
    fn recorded_wifi_fixture_lists_expected_aps() {
        let aps: Vec<ApRead> = serde_json::from_str(FIXTURE).unwrap();
        let vm = vm(read(
            vec![device("wlan0", 2, 30, aps)],
            vec![KnownRead {
                uuid: "u-1".into(),
                id: "fixture-open".into(),
                kind: "802-11-wireless".into(),
                autoconnect: true,
                ssid: Some(b"fixture-open".to_vec()),
            }],
        ));
        let got: Vec<(String, u8, String, bool, bool)> = vm
            .state()
            .access_points
            .into_iter()
            .map(|a| (a.ssid, a.strength, a.security, a.active, a.known))
            .collect();
        let want = [
            ("fantastic-ci", 94, "wpa-psk", true, false),
            ("fixture-sae", 81, "sae", false, false),
            ("fixture-open", 70, "open", false, true),
            ("fixture-corp", 62, "enterprise", false, false),
            ("0xc3fe00ff", 48, "wpa-psk", false, false),
            ("fixture-wep", 33, "wep", false, false),
        ]
        .map(|(s, st, sec, act, kn)| (s.to_string(), st, sec.to_string(), act, kn));
        assert_eq!(got, want.to_vec());
    }

    fn pair() -> NetworkVm {
        vm(read(
            vec![device(
                "wlan0",
                2,
                30,
                vec![
                    ap("/ap/open", b"cafe", 80, 0, 0, 0),
                    ap("/ap/psk", b"home", 70, 1, 0, 0x188),
                    ap("/ap/sae", b"safe", 60, 1, 0, KEY_MGMT_SAE | 0x8),
                    ap("/ap/known", b"work", 50, 1, 0, 0x188),
                    ap("/ap/wep", b"old", 40, 1, 0, 0),
                    ap("/ap/corp", b"corp", 30, 1, 0, KEY_MGMT_802_1X | 0x8),
                ],
            )],
            vec![KnownRead {
                uuid: "uuid-work".into(),
                id: "work".into(),
                kind: "802-11-wireless".into(),
                autoconnect: true,
                ssid: Some(b"work".to_vec()),
            }],
        ))
    }

    #[test]
    fn connect_to_known_ssid_asks_activate_and_unknown_builds_settings_tree() {
        let mut vm = pair();
        // known: activate, even with no password
        assert_eq!(
            intent(&mut vm, "connect", ssid_arg("work")),
            Effect::Call(Call::Activate {
                uuid: "uuid-work".into(),
                iface: "wlan0".into()
            })
        );
        // unknown WPA2
        let Effect::Call(Call::ConnectNew {
            iface,
            ap,
            settings,
        }) = intent(&mut vm, "connect", with_password("home", "correct horse"))
        else {
            panic!("a WPA2 network with a password connects")
        };
        assert_eq!((iface.as_str(), ap.as_str()), ("wlan0", "/ap/psk"));
        let s = |v: &str| Setting::Str(v.into());
        let want: BTreeMap<String, BTreeMap<String, Setting>> = BTreeMap::from([
            (
                "connection".into(),
                BTreeMap::from([
                    ("type".into(), s("802-11-wireless")),
                    ("id".into(), s("home")),
                    (
                        "permissions".into(),
                        Setting::StrList(vec!["user:fantastic".into()]),
                    ),
                ]),
            ),
            (
                "802-11-wireless".into(),
                BTreeMap::from([
                    ("ssid".into(), Setting::Bytes(b"home".to_vec())),
                    ("mode".into(), s("infrastructure")),
                ]),
            ),
            (
                "802-11-wireless-security".into(),
                BTreeMap::from([
                    ("key-mgmt".into(), s("wpa-psk")),
                    ("psk".into(), s("correct horse")),
                ]),
            ),
        ]);
        assert_eq!(settings.0, want);
        // sae
        let Effect::Call(Call::ConnectNew { settings, .. }) =
            intent(&mut vm, "connect", with_password("safe", "12345678"))
        else {
            panic!("sae connects")
        };
        assert_eq!(
            settings.0["802-11-wireless-security"]["key-mgmt"],
            Setting::Str("sae".into())
        );
        // open: no security section, and a password given is not used
        let Effect::Call(Call::ConnectNew { settings, .. }) =
            intent(&mut vm, "connect", with_password("cafe", "ignored!!"))
        else {
            panic!("open connects")
        };
        assert!(!settings.0.contains_key("802-11-wireless-security"));
        assert!(!format!("{settings:?}").contains("ignored"));
        assert_eq!(
            settings.0["connection"]["permissions"],
            Setting::StrList(vec!["user:fantastic".into()])
        );
    }

    #[test]
    fn connect_is_refused_for_enterprise_wep_missing_or_bad_length_password() {
        let mut vm = pair();
        let corp = intent(&mut vm, "connect", with_password("corp", "whatever1"));
        assert_eq!(reason(&corp), "unsupported");
        assert!(message(&corp).contains("enterprise"), "{}", message(&corp));
        let wep = intent(&mut vm, "connect", with_password("old", "whatever1"));
        assert_eq!(reason(&wep), "unsupported");
        assert!(message(&wep).contains("wep"));
        let none = intent(&mut vm, "connect", ssid_arg("home"));
        assert_eq!(
            (reason(&none), message(&none)),
            ("bad_arg", "connect: `home` needs a `password`")
        );
        for bad in ["1234567", &"x".repeat(64), &"x".repeat(65)] {
            let e = intent(&mut vm, "connect", with_password("home", bad));
            assert_eq!(reason(&e), "bad_arg", "{bad:?}");
            assert!(message(&e).contains("8 to 63"));
        }
        // 64 hex digits and the 8 / 63 limits are fine
        for ok in ["12345678".to_string(), "y".repeat(63), "ab".repeat(32)] {
            assert!(
                matches!(
                    intent(&mut vm, "connect", with_password("home", &ok)),
                    Effect::Call(_)
                ),
                "{ok:?}"
            );
        }
        assert_eq!(
            reason(&intent(&mut vm, "connect", Args::default())),
            "bad_arg"
        );
        assert_eq!(
            reason(&intent(&mut vm, "connect", ssid_arg("nowhere"))),
            "not_found"
        );
    }

    #[test]
    fn connect_and_scan_are_refused_while_wireless_is_off() {
        let mut r = read(
            vec![device("wlan0", 2, 20, vec![ap("/a", b"cafe", 80, 0, 0, 0)])],
            vec![],
        );
        r.wireless_enabled = false;
        let mut vm = vm(r);
        assert_eq!(
            reason(&intent(&mut vm, "connect", ssid_arg("cafe"))),
            "unavailable"
        );
        assert_eq!(
            reason(&intent(&mut vm, "scan", Args::default())),
            "unavailable"
        );
        let mut wired = self::vm(read(vec![device("enp0s2", 1, 100, vec![])], vec![]));
        assert_eq!(
            reason(&intent(&mut wired, "scan", Args::default())),
            "not_found"
        );
    }

    #[test]
    fn password_never_appears_in_state_or_debug_output() {
        let secret = "hunter2-hunter2";
        let mut vm = pair();
        let ev = Event::Intent {
            verb: "connect".into(),
            args: with_password("home", secret),
        };
        assert!(!format!("{ev:?}").contains(secret), "the event's Debug");
        let effects = vm.reduce(ev);
        assert!(matches!(effects[0], Effect::Call(Call::ConnectNew { .. })));
        assert!(
            !format!("{effects:?}").contains(secret),
            "the effect's Debug"
        );
        assert!(
            !format!("{:?}", effects[0]).contains("psk: Str"),
            "psk is redacted by name"
        );
        assert!(!serde_json::to_string(&vm.state()).unwrap().contains(secret));
        assert!(!format!("{:?}", vm.state()).contains(secret));
        // the refused ones neither
        let bad = vm.reduce(Event::Intent {
            verb: "connect".into(),
            args: with_password("old", secret),
        });
        assert!(!format!("{bad:?}").contains(secret));
    }

    #[test]
    fn intents_deactivate_forget_set_wireless_refuse_unknown_targets() {
        let mut r = read(
            vec![
                device("enp0s2", 1, 100, vec![]),
                device("wlan0", 2, 30, vec![]),
            ],
            vec![
                KnownRead {
                    uuid: "u-eth".into(),
                    id: "Wired connection 1".into(),
                    kind: "802-3-ethernet".into(),
                    autoconnect: true,
                    ssid: None,
                },
                KnownRead {
                    uuid: "u-vpn".into(),
                    id: "vpn".into(),
                    kind: "vpn".into(),
                    autoconnect: false,
                    ssid: None,
                },
            ],
        );
        r.devices[0].connection = "Wired connection 1".into();
        let mut vm = vm(r);
        let iface = |i: &str| Args {
            iface: Some(i.into()),
            ..Args::default()
        };
        let uuid = |u: &str| Args {
            uuid: Some(u.into()),
            ..Args::default()
        };
        assert_eq!(
            intent(&mut vm, "deactivate", iface("enp0s2")),
            Effect::Call(Call::Deactivate {
                iface: "enp0s2".into()
            })
        );
        assert_eq!(
            reason(&intent(&mut vm, "deactivate", iface("eth9"))),
            "not_found"
        );
        assert_eq!(
            reason(&intent(&mut vm, "deactivate", iface("wlan0"))),
            "not_found"
        );
        assert_eq!(
            reason(&intent(&mut vm, "deactivate", Args::default())),
            "bad_arg"
        );
        assert_eq!(
            intent(&mut vm, "forget", uuid("u-eth")),
            Effect::Call(Call::Forget {
                uuid: "u-eth".into()
            })
        );
        assert_eq!(
            reason(&intent(&mut vm, "forget", uuid("nope"))),
            "not_found"
        );
        assert_eq!(
            reason(&intent(&mut vm, "forget", Args::default())),
            "bad_arg"
        );
        assert_eq!(
            intent(&mut vm, "activate", uuid("u-eth")),
            Effect::Call(Call::Activate {
                uuid: "u-eth".into(),
                iface: "enp0s2".into()
            })
        );
        assert_eq!(
            reason(&intent(&mut vm, "activate", uuid("u-vpn"))),
            "not_found"
        );
        assert_eq!(
            reason(&intent(&mut vm, "activate", uuid("nope"))),
            "not_found"
        );
        let enabled = |e| Args {
            enabled: Some(e),
            ..Args::default()
        };
        assert_eq!(
            intent(&mut vm, "set_wireless", enabled(false)),
            Effect::Call(Call::SetWireless { enabled: false })
        );
        assert_eq!(
            reason(&intent(&mut vm, "set_wireless", Args::default())),
            "bad_arg"
        );
        assert_eq!(
            reason(&intent(&mut vm, "nope", Args::default())),
            "unsupported"
        );
    }

    #[test]
    fn set_wireless_on_is_refused_when_the_hardware_switch_is_off() {
        let mut r = read(vec![], vec![]);
        r.wireless_hw_enabled = false;
        let mut vm = vm(r);
        let on = Args {
            enabled: Some(true),
            ..Args::default()
        };
        assert_eq!(reason(&intent(&mut vm, "set_wireless", on)), "unsupported");
    }

    #[test]
    fn state_reads_devices_known_and_hides_loopback() {
        let mut eth = device("enp0s2", 1, 100, vec![]);
        eth.connection = "Wired connection 1".into();
        eth.ip4 = vec!["10.0.2.15/24".into()];
        let vm = vm(read(vec![device("lo", 14, 10, vec![]), eth], vec![]));
        let v = serde_json::to_value(vm.state()).unwrap();
        assert_eq!(v["state"], "connected_global");
        assert_eq!(v["connectivity"], "full");
        assert_eq!(v["devices"].as_array().unwrap().len(), 1);
        assert_eq!(v["devices"][0]["kind"], "ethernet");
        assert_eq!(v["devices"][0]["state"], "activated");
        assert_eq!(v["devices"][0]["ip4"][0], "10.0.2.15/24");
        assert_eq!(
            v["wireless"],
            serde_json::json!({"enabled": true, "hardware_enabled": true})
        );
        assert!(v["busy"].is_null());
    }

    #[test]
    fn unavailable_before_a_read_and_after_a_failure_and_changed_or_a_tick_reread() {
        let mut vm = NetworkVm::new(Config::default());
        let s = vm.state();
        assert!(!s.available);
        assert_eq!(s.why.as_deref(), Some("not read yet"));
        let e = intent(&mut vm, "scan", Args::default());
        assert_eq!(reason(&e), "unavailable");
        assert_eq!(vm.reduce(Event::Changed), vec![Effect::Read]);
        vm.reduce(Event::Read(read(vec![], vec![])));
        assert!(vm.state().available);
        vm.reduce(Event::Unavailable {
            why: "org.freedesktop.NetworkManager is not on the system bus".into(),
        });
        let s = vm.state();
        assert!(!s.available && s.devices.is_empty());
        assert_eq!(
            reason(&intent(&mut vm, "forget", Args::default())),
            "unavailable"
        );
        assert!(vm.reduce(Event::Tick { now_ms: 1_000 }).is_empty());
        assert!(vm.reduce(Event::Tick { now_ms: 20_000 }).is_empty());
        assert_eq!(
            vm.reduce(Event::Tick { now_ms: 31_000 }),
            vec![Effect::Read]
        );
        assert!(vm.reduce(Event::Tick { now_ms: 32_000 }).is_empty());
    }
}
