//! POWER on the system bus: UPower (battery, lid) and logind (the `Can*`
//! answers, the power methods, `PrepareForSleep`).

use fantastic_system::power::{Call, Can, Event, Read};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

use crate::sys::linux::os_error;
use crate::sys::{OsError, PowerOs};

const UPOWER: &str = "org.freedesktop.UPower";
const LOGIND: &str = "org.freedesktop.login1";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";

#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
trait UPower {
    fn get_display_device(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn lid_is_closed(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn lid_is_present(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower"
)]
trait Device {
    #[zbus(property)]
    fn is_present(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "Type")]
    fn kind(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn time_to_empty(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn time_to_full(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn warning_level(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    fn can_power_off(&self) -> zbus::Result<String>;
    fn can_reboot(&self) -> zbus::Result<String>;
    fn can_suspend(&self) -> zbus::Result<String>;
    fn can_hibernate(&self) -> zbus::Result<String>;
    fn power_off(&self, interactive: bool) -> zbus::Result<()>;
    fn reboot(&self, interactive: bool) -> zbus::Result<()>;
    fn suspend(&self, interactive: bool) -> zbus::Result<()>;
    fn hibernate(&self, interactive: bool) -> zbus::Result<()>;
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// UPower and logind on a given system-bus connection.
pub struct LinuxPower {
    conn: zbus::Connection,
}

impl LinuxPower {
    pub fn new(system: zbus::Connection) -> Self {
        LinuxPower { conn: system }
    }

    async fn upower(&self) -> zbus::Result<UPowerProxy<'_>> {
        UPowerProxy::builder(&self.conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    async fn manager(&self) -> zbus::Result<ManagerProxy<'_>> {
        ManagerProxy::builder(&self.conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    async fn read_upower(&self) -> Result<Read, OsError> {
        let e = |e| os_error(e, UPOWER);
        let up = self.upower().await.map_err(e)?;
        let display = up.get_display_device().await.map_err(e)?;
        let dev = DeviceProxy::builder(&self.conn)
            .path(display)
            .map_err(e)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(e)?;
        let display_present = dev.is_present().await.map_err(e)?;
        let mut r = Read {
            on_battery: up.on_battery().await.map_err(e)?,
            lid_present: up.lid_is_present().await.map_err(e)?,
            lid_closed: up.lid_is_closed().await.map_err(e)?,
            display_present,
            display_type: 0,
            display_state: 0,
            percentage: 0.0,
            time_to_empty: 0,
            time_to_full: 0,
            warning: 0,
            can: Can {
                power_off: String::new(),
                reboot: String::new(),
                suspend: String::new(),
                hibernate: String::new(),
            },
        };
        if display_present {
            r.display_type = dev.kind().await.map_err(e)?;
            r.display_state = dev.state().await.map_err(e)?;
            r.percentage = dev.percentage().await.map_err(e)?;
            r.time_to_empty = dev.time_to_empty().await.map_err(e)?;
            r.time_to_full = dev.time_to_full().await.map_err(e)?;
            r.warning = dev.warning_level().await.map_err(e)?;
        }
        Ok(r)
    }

    async fn read_can(&self) -> Result<Can, OsError> {
        let e = |e| os_error(e, LOGIND);
        let m = self.manager().await.map_err(e)?;
        Ok(Can {
            power_off: m.can_power_off().await.map_err(e)?,
            reboot: m.can_reboot().await.map_err(e)?,
            suspend: m.can_suspend().await.map_err(e)?,
            hibernate: m.can_hibernate().await.map_err(e)?,
        })
    }
}

#[async_trait::async_trait]
impl PowerOs for LinuxPower {
    async fn read(&self) -> Result<Read, OsError> {
        let mut r = self.read_upower().await?;
        r.can = self.read_can().await?;
        Ok(r)
    }

    async fn call(&self, c: Call) -> Result<(), OsError> {
        let e = |e| os_error(e, LOGIND);
        let m = self.manager().await.map_err(e)?;
        // never interactive: there is no polkit agent to ask
        match c {
            Call::PowerOff => m.power_off(false).await,
            Call::Reboot => m.reboot(false).await,
            Call::Suspend => m.suspend(false).await,
            Call::Hibernate => m.hibernate(false).await,
        }
        .map_err(e)
    }

    fn changes(&self) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(64);
        let conn = self.conn.clone();
        tokio::spawn(async move {
            // UPower: the root object and the display device; logind: sleep
            let up = forward_properties(conn.clone(), UPOWER, UPOWER_PATH.to_string(), tx.clone());
            let dev = forward_properties(
                conn.clone(),
                UPOWER,
                "/org/freedesktop/UPower/devices/DisplayDevice".to_string(),
                tx.clone(),
            );
            let sleep = forward_sleep(conn, tx);
            tokio::join!(up, dev, sleep);
        });
        rx
    }
}

/// `PropertiesChanged` of one object, as `Changed` events, until the receiver is dropped.
async fn forward_properties(
    conn: zbus::Connection,
    service: &'static str,
    path: String,
    tx: mpsc::Sender<Event>,
) {
    let Ok(builder) = zbus::fdo::PropertiesProxy::builder(&conn).destination(service) else {
        return;
    };
    let Ok(builder) = builder.path(path) else {
        return;
    };
    let Ok(proxy) = builder.build().await else {
        return;
    };
    let Ok(mut stream) = proxy.receive_properties_changed().await else {
        return;
    };
    loop {
        tokio::select! {
            _ = tx.closed() => return,
            signal = stream.next() => match signal {
                Some(_) => {
                    if tx.send(Event::Changed).await.is_err() {
                        return;
                    }
                }
                None => return,
            },
        }
    }
}

/// logind's `PrepareForSleep`, as events.
async fn forward_sleep(conn: zbus::Connection, tx: mpsc::Sender<Event>) {
    let Ok(m) = ManagerProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
    else {
        return;
    };
    let Ok(mut stream) = m.receive_prepare_for_sleep().await else {
        return;
    };
    loop {
        tokio::select! {
            _ = tx.closed() => return,
            signal = stream.next() => match signal {
                Some(s) => {
                    let Ok(args) = s.args() else { continue };
                    if tx.send(Event::PrepareForSleep { start: args.start }).await.is_err() {
                        return;
                    }
                }
                None => return,
            },
        }
    }
}
