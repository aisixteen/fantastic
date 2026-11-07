//! A PRIVATE D-BUS with python-dbusmock templates on it (Linux; needs
//! `dbus-daemon` and `python3-dbusmock`). One `dbus-daemon` on a temp config
//! that allows everything; each template runs as a child with
//! `DBUS_SYSTEM_BUS_ADDRESS` (and the session one) pointed at it; `start`
//! returns once every template's name is owned. Everything is killed on drop.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Makes each bus of a test binary its own directory (tests run in parallel).
static COUNT: AtomicUsize = AtomicUsize::new(0);

pub struct Bus {
    daemon: Child,
    mocks: Vec<Child>,
    pub address: String,
    dir: PathBuf,
}

/// The well-known name a dbusmock template owns.
fn name_of(template: &str) -> &'static str {
    match template {
        "upower" => "org.freedesktop.UPower",
        "logind" => "org.freedesktop.login1",
        "networkmanager" => "org.freedesktop.NetworkManager",
        other => panic!("support/bus.rs does not know the template `{other}`"),
    }
}

impl Bus {
    /// Start the daemon and the templates (`-p` parameters as JSON, optional).
    pub async fn start(templates: &[(&str, Option<&str>)]) -> Bus {
        let dir = std::env::temp_dir().join(format!(
            "fantastic_bus_{}_{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}/bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.display()
            ),
        )
        .unwrap();
        let mut daemon = Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .arg("--nofork")
            .arg("--print-address=1")
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon is installed");
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_string();
        assert!(
            address.starts_with("unix:"),
            "dbus-daemon printed {address:?}"
        );

        let mut bus = Bus {
            daemon,
            mocks: vec![],
            address,
            dir,
        };
        for (template, params) in templates {
            let mut cmd = Command::new("python3");
            cmd.args(["-m", "dbusmock", "--system", "--template", template]);
            if let Some(p) = params {
                cmd.args(["-p", p]);
            }
            let child = cmd
                .env("DBUS_SYSTEM_BUS_ADDRESS", &bus.address)
                .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("python3 -m dbusmock runs");
            bus.mocks.push(child);
        }
        let conn = bus.connect().await;
        let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
        for (template, _) in templates {
            let name = zbus::names::BusName::try_from(name_of(template)).unwrap();
            let start = Instant::now();
            while !dbus.name_has_owner(name.clone()).await.unwrap() {
                assert!(
                    start.elapsed() < Duration::from_secs(20),
                    "{template} never owned {name}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        bus
    }

    /// A connection to the private bus.
    pub async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .expect("a bus address")
            .build()
            .await
            .expect("connects to the private bus")
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        for m in &mut self.mocks {
            let _ = m.kill();
            let _ = m.wait();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
