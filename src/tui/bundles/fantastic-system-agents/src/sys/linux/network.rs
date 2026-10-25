//! NETWORK on the system bus: NetworkManager's devices, Wi-Fi access points
//! and saved connections, flattened into a `Read`; the `Call`s become its
//! methods. References: the NetworkManager D-Bus API (`org.freedesktop.NetworkManager`,
//! `.Device`, `.Device.Wireless`, `.AccessPoint`, `.IP4Config`,
//! `.Settings`, `.Settings.Connection`, `.Connection.Active`).

use std::collections::HashMap;

use fantastic_system::network::{
    ApRead, Call, DeviceRead, Event, KnownRead, Read, Setting, Settings,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{Array, ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::sys::linux::os_error;
use crate::sys::{NetworkOs, OsError};

const NM: &str = "org.freedesktop.NetworkManager";
const NO_OBJECT: &str = "/";
/// `NMDeviceType`: Wi-Fi.
const WIFI: u32 = 2;

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn activate_connection(
        &self,
        connection: &ObjectPath<'_>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn add_and_activate_connection(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;
    fn deactivate_connection(&self, active_connection: &ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn connectivity(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn wireless_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_wireless_enabled(&self, enabled: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn wireless_hardware_enabled(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmDevice {
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn active_connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmWireless {
    fn get_all_access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn request_scan(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.AccessPoint",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmAccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn wpa_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn rsn_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn frequency(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.IP4Config",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmIp4Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmActive {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Settings",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings"
)]
trait NmSettings {
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmSettingsConnection {
    fn get_settings(&self) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;
    fn delete(&self) -> zbus::Result<()>;
}

/// NetworkManager on a given system-bus connection.
pub struct LinuxNetwork {
    conn: zbus::Connection,
}

/// A setting as a D-Bus variant.
fn variant(s: &Setting) -> Value<'_> {
    match s {
        Setting::Str(v) => Value::from(v.as_str()),
        Setting::Bytes(v) => Value::Array(Array::from(v.clone())),
        Setting::Bool(v) => Value::from(*v),
        Setting::U32(v) => Value::from(*v),
        Setting::StrList(v) => Value::Array(Array::from(v.clone())),
    }
}

/// A text property of a settings section.
fn text(section: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let v = section.get(key)?.try_clone().ok()?;
    String::try_from(v).ok()
}

impl LinuxNetwork {
    pub fn new(system: zbus::Connection) -> Self {
        LinuxNetwork { conn: system }
    }

    async fn nm(&self) -> zbus::Result<NetworkManagerProxy<'_>> {
        NetworkManagerProxy::builder(&self.conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    async fn device(&self, path: &OwnedObjectPath) -> zbus::Result<NmDeviceProxy<'_>> {
        NmDeviceProxy::builder(&self.conn)
            .path(path.clone())?
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    async fn connection_settings(
        &self,
        path: &OwnedObjectPath,
    ) -> zbus::Result<NmSettingsConnectionProxy<'_>> {
        NmSettingsConnectionProxy::builder(&self.conn)
            .path(path.clone())?
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    /// The `ip/prefix` strings of an IP4Config object; none for `/` or on any failure.
    async fn ip4(&self, path: &OwnedObjectPath) -> Vec<String> {
        if path.as_str() == NO_OBJECT {
            return vec![];
        }
        let Ok(builder) = NmIp4ConfigProxy::builder(&self.conn).path(path.clone()) else {
            return vec![];
        };
        let Ok(cfg) = builder.cache_properties(CacheProperties::No).build().await else {
            return vec![];
        };
        let rows = cfg.address_data().await.unwrap_or_default();
        rows.iter()
            .filter_map(|row| {
                let address = String::try_from(row.get("address")?.try_clone().ok()?).ok()?;
                let prefix = u32::try_from(row.get("prefix")?.try_clone().ok()?).ok()?;
                Some(format!("{address}/{prefix}"))
            })
            .collect()
    }

    async fn access_points(&self, path: &OwnedObjectPath) -> zbus::Result<Vec<ApRead>> {
        let wifi = NmWirelessProxy::builder(&self.conn)
            .path(path.clone())?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let active = wifi.active_access_point().await.ok();
        let mut out = Vec::new();
        for ap_path in wifi.get_all_access_points().await? {
            let ap = NmAccessPointProxy::builder(&self.conn)
                .path(ap_path.clone())?
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            // an AP that vanished between the list and the read is skipped
            let (Ok(ssid), Ok(strength)) = (ap.ssid().await, ap.strength().await) else {
                continue;
            };
            out.push(ApRead {
                path: ap_path.to_string(),
                ssid,
                strength,
                flags: ap.flags().await.unwrap_or(0),
                wpa_flags: ap.wpa_flags().await.unwrap_or(0),
                rsn_flags: ap.rsn_flags().await.unwrap_or(0),
                frequency: ap.frequency().await.unwrap_or(0),
                active: active.as_ref() == Some(&ap_path) && ap_path.as_str() != NO_OBJECT,
            });
        }
        Ok(out)
    }

    async fn read_devices(&self, nm: &NetworkManagerProxy<'_>) -> Result<Vec<DeviceRead>, OsError> {
        let e = |e| os_error(e, NM);
        let mut devices = Vec::new();
        for path in nm.get_devices().await.map_err(e)? {
            let Ok(dev) = self.device(&path).await else {
                continue;
            };
            let (Ok(iface), Ok(device_type), Ok(state)) = (
                dev.interface().await,
                dev.device_type().await,
                dev.state().await,
            ) else {
                continue;
            };
            let connection = match dev.active_connection().await {
                Ok(active) if active.as_str() != NO_OBJECT => {
                    match NmActiveProxy::builder(&self.conn).path(active) {
                        Ok(b) => match b.cache_properties(CacheProperties::No).build().await {
                            Ok(a) => a.id().await.unwrap_or_default(),
                            Err(_) => String::new(),
                        },
                        Err(_) => String::new(),
                    }
                }
                _ => String::new(),
            };
            let ip4 = match dev.ip4_config().await {
                Ok(p) => self.ip4(&p).await,
                Err(_) => vec![],
            };
            let aps = if device_type == WIFI {
                self.access_points(&path).await.unwrap_or_default()
            } else {
                vec![]
            };
            devices.push(DeviceRead {
                path: path.to_string(),
                iface,
                device_type,
                state,
                connection,
                ip4,
                aps,
            });
        }
        Ok(devices)
    }

    async fn settings_paths(&self) -> zbus::Result<Vec<OwnedObjectPath>> {
        NmSettingsProxy::builder(&self.conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await?
            .list_connections()
            .await
    }

    async fn read_known(&self) -> Result<Vec<KnownRead>, OsError> {
        let e = |e| os_error(e, NM);
        let mut known = Vec::new();
        for path in self.settings_paths().await.map_err(e)? {
            let Ok(conn) = self.connection_settings(&path).await else {
                continue;
            };
            let Ok(settings) = conn.get_settings().await else {
                continue;
            };
            let Some(c) = settings.get("connection") else {
                continue;
            };
            let (Some(uuid), Some(id), Some(kind)) =
                (text(c, "uuid"), text(c, "id"), text(c, "type"))
            else {
                continue;
            };
            let autoconnect = c
                .get("autoconnect")
                .and_then(|v| v.try_clone().ok())
                .and_then(|v| bool::try_from(v).ok())
                .unwrap_or(true);
            let ssid = settings
                .get("802-11-wireless")
                .and_then(|w| w.get("ssid"))
                .and_then(|v| v.try_clone().ok())
                .and_then(|v| Vec::<u8>::try_from(v).ok());
            known.push(KnownRead {
                uuid,
                id,
                kind,
                autoconnect,
                ssid,
            });
        }
        Ok(known)
    }

    /// The object path of the saved connection with this uuid.
    async fn connection_of(&self, uuid: &str) -> Result<OwnedObjectPath, OsError> {
        let e = |e| os_error(e, NM);
        for path in self.settings_paths().await.map_err(e)? {
            let Ok(conn) = self.connection_settings(&path).await else {
                continue;
            };
            let found = conn
                .get_settings()
                .await
                .ok()
                .and_then(|s| s.get("connection").and_then(|c| text(c, "uuid")));
            if found.as_deref() == Some(uuid) {
                return Ok(path);
            }
        }
        Err(OsError::Failed(format!("no saved connection {uuid}")))
    }

    /// The object path of the device with this interface name.
    async fn device_of(&self, iface: &str) -> Result<OwnedObjectPath, OsError> {
        let e = |e| os_error(e, NM);
        let nm = self.nm().await.map_err(e)?;
        for path in nm.get_devices().await.map_err(e)? {
            let Ok(dev) = self.device(&path).await else {
                continue;
            };
            if dev.interface().await.ok().as_deref() == Some(iface) {
                return Ok(path);
            }
        }
        Err(OsError::Failed(format!("no device {iface}")))
    }
}

#[async_trait::async_trait]
impl NetworkOs for LinuxNetwork {
    async fn read(&self) -> Result<Read, OsError> {
        let e = |e| os_error(e, NM);
        let nm = self.nm().await.map_err(e)?;
        let state = nm.state().await.map_err(e)?;
        Ok(Read {
            state,
            connectivity: nm.connectivity().await.unwrap_or(0),
            wireless_enabled: nm.wireless_enabled().await.unwrap_or(false),
            wireless_hw_enabled: nm.wireless_hardware_enabled().await.unwrap_or(false),
            devices: self.read_devices(&nm).await?,
            known: self.read_known().await?,
        })
    }

    async fn call(&self, c: Call) -> Result<(), OsError> {
        let e = |e| os_error(e, NM);
        let nm = self.nm().await.map_err(e)?;
        let none = ObjectPath::from_static_str_unchecked(NO_OBJECT);
        match c {
            Call::Scan => {
                let mut tried = 0;
                let mut last = None;
                for path in nm.get_devices().await.map_err(e)? {
                    let Ok(dev) = self.device(&path).await else {
                        continue;
                    };
                    if dev.device_type().await.ok() != Some(WIFI) {
                        continue;
                    }
                    let wifi = NmWirelessProxy::builder(&self.conn)
                        .path(path.clone())
                        .map_err(e)?
                        .cache_properties(CacheProperties::No)
                        .build()
                        .await
                        .map_err(e)?;
                    match wifi.request_scan(HashMap::new()).await {
                        Ok(()) => tried += 1,
                        Err(err) => last = Some(err),
                    }
                }
                // NetworkManager refuses a scan right after another: that is not a failure
                match (tried, last) {
                    (0, Some(err)) => Err(e(err)),
                    _ => Ok(()),
                }
            }
            Call::Activate { uuid, iface } => {
                let conn = self.connection_of(&uuid).await?;
                let dev = self.device_of(&iface).await?;
                nm.activate_connection(&conn, &dev, &none)
                    .await
                    .map(|_| ())
                    .map_err(e)
            }
            Call::ConnectNew {
                iface,
                ap,
                settings,
            } => {
                let dev = self.device_of(&iface).await?;
                let ap = ObjectPath::try_from(ap.as_str())
                    .map_err(|x| OsError::Failed(format!("bad access point path: {x}")))?;
                let tree = tree_of(&settings);
                nm.add_and_activate_connection(tree, &dev, &ap)
                    .await
                    .map(|_| ())
                    .map_err(e)
            }
            Call::Deactivate { iface } => {
                let dev = self.device_of(&iface).await?;
                let active = self
                    .device(&dev)
                    .await
                    .map_err(e)?
                    .active_connection()
                    .await
                    .map_err(e)?;
                if active.as_str() == NO_OBJECT {
                    return Err(OsError::Failed(format!("{iface} has no active connection")));
                }
                nm.deactivate_connection(&active).await.map_err(e)
            }
            Call::Forget { uuid } => {
                let path = self.connection_of(&uuid).await?;
                self.connection_settings(&path)
                    .await
                    .map_err(e)?
                    .delete()
                    .await
                    .map_err(e)
            }
            Call::SetWireless { enabled } => nm.set_wireless_enabled(enabled).await.map_err(e),
        }
    }

    fn changes(&self) -> mpsc::Receiver<Event> {
        // one slot: a burst of signals is one re-read, never a queue of them
        let (tx, rx) = mpsc::channel(1);
        let conn = self.conn.clone();
        tokio::spawn(async move {
            let Ok(rule) = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(NM)
                .map(|b| b.build())
            else {
                return;
            };
            let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &conn, Some(64)).await
            else {
                return;
            };
            loop {
                tokio::select! {
                    _ = tx.closed() => return,
                    msg = stream.next() => match msg {
                        Some(_) => {
                            // a full slot is a read already asked for
                            let _ = tx.try_send(Event::Changed);
                        }
                        None => return,
                    },
                }
            }
        });
        rx
    }
}

/// The pure settings tree as `a{sa{sv}}`.
fn tree_of(settings: &Settings) -> HashMap<&str, HashMap<&str, Value<'_>>> {
    settings
        .0
        .iter()
        .map(|(section, keys)| {
            (
                section.as_str(),
                keys.iter().map(|(k, v)| (k.as_str(), variant(v))).collect(),
            )
        })
        .collect()
}
