//! THE LOCAL CHANNELS (MCP, the shared terminal view), the same on every OS: a
//! loopback TCP port, found through an endpoint file in `<state>/cli/`.
//!
//! No socket files. The file `{pid, port, token}` is written whole, once the
//! listener is bound, and PRIVATE from its first byte (`0o600`): it holds a
//! TOKEN a connection must open with. Any local user can reach a loopback port;
//! only this user can read the file. A connection that does not open with the
//! token is dropped. Removing the file is closing. A client reads the file,
//! refuses it when its pid is gone (a stale file), connects and sends the token.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The MCP endpoint's file, in `<state>/cli/`.
pub(crate) const MCP: &str = "mcp.yaml";
/// The shared terminal view's endpoint file, in `<state>/cli/`.
pub(crate) const TUI: &str = "tui.yaml";

/// How long a new connection has to send the token before it is dropped, so a
/// stranger that connects and says nothing holds up the next client no longer.
const TOKEN_WAIT: Duration = Duration::from_secs(2);

/// What the endpoint file says.
#[derive(Serialize, Deserialize)]
struct Endpoint {
    pid: u32,
    port: u16,
    token: String,
}

/// The listener: a loopback port, and the token its connections open with.
pub(crate) struct Listener {
    tcp: TcpListener,
    token: String,
}

/// Serve a channel: listen on a free loopback port, then write `<cli>/<name>`
/// (`{pid, port, token}`), private before its first byte and whole or not at all.
pub(crate) fn bind(cli: &Path, name: &str) -> io::Result<Listener> {
    let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    l.set_nonblocking(true)?;
    let tcp = TcpListener::from_std(l)?;
    let token = fantastic_kernel::lifecycle::token_hex(16);
    let ep = Endpoint {
        pid: std::process::id(),
        port: tcp.local_addr()?.port(),
        token: token.clone(),
    };
    let text = serde_yaml::to_string(&ep).map_err(io::Error::other)?;
    std::fs::create_dir_all(cli)?;
    fantastic_file::fs::write_bytes(cli, name, text.as_bytes(), true, Some(0o600))
        .map_err(io::Error::other)?;
    Ok(Listener { tcp, token })
}

impl Listener {
    /// The next connection that opened with the token; any other is dropped
    /// unanswered.
    pub(crate) async fn accept(&self) -> io::Result<TcpStream> {
        loop {
            let (mut s, _peer) = self.tcp.accept().await?;
            let mut got = vec![0u8; self.token.len()];
            let sent = tokio::time::timeout(TOKEN_WAIT, s.read_exact(&mut got)).await;
            // small frames: no Nagle delay
            if matches!(sent, Ok(Ok(_)))
                && same(&got, self.token.as_bytes())
                && s.set_nodelay(true).is_ok()
            {
                return Ok(s);
            }
        }
    }
}

/// Equal, in a time that does not tell where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Connect to the channel `<cli>/<name>`: the port its file names, opened with
/// the file's token. A file whose pid is gone is stale and refused.
pub(crate) async fn connect(cli: &Path, name: &str) -> io::Result<TcpStream> {
    let file = cli.join(name);
    let text = std::fs::read_to_string(&file)?;
    let ep: Endpoint = serde_yaml::from_str(&text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} names no pid, port and token", file.display()),
        )
    })?;
    if !fantastic_kernel::lock::pid_alive(ep.pid) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("stale: pid {} is gone", ep.pid),
        ));
    }
    let mut s = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, ep.port))).await?;
    s.set_nodelay(true)?;
    s.write_all(ep.token.as_bytes()).await?;
    Ok(s)
}

/// Close a channel: remove its endpoint file.
pub(crate) fn close(cli: &Path, name: &str) {
    let _ = std::fs::remove_file(cli.join(name));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fc-local-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn the_endpoint_file_is_private_and_names_pid_port_token() {
        let cli = scratch("file").join("cli");
        rt().block_on(async {
            let _l = bind(&cli, "x.yaml").unwrap();
            let ep: Endpoint =
                serde_yaml::from_str(&std::fs::read_to_string(cli.join("x.yaml")).unwrap())
                    .unwrap();
            assert_eq!(ep.pid, std::process::id());
            assert!(ep.port > 0);
            assert_eq!(ep.token.len(), 32);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(cli.join("x.yaml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600);
            }
        });
        close(&cli, "x.yaml");
        assert!(!cli.join("x.yaml").exists());
    }

    #[test]
    fn a_wrong_or_missing_token_is_dropped() {
        let cli = scratch("bad").join("cli");
        rt().block_on(async {
            let l = bind(&cli, "x.yaml").unwrap();
            let ep: Endpoint =
                serde_yaml::from_str(&std::fs::read_to_string(cli.join("x.yaml")).unwrap())
                    .unwrap();
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, ep.port));
            // a wrong token of the right length: dropped, the reader gets EOF
            let mut bad = TcpStream::connect(addr).await.unwrap();
            bad.write_all("0".repeat(ep.token.len()).as_bytes())
                .await
                .unwrap();
            // a stranger that says nothing
            let mut mute = TcpStream::connect(addr).await.unwrap();
            let accepted = tokio::time::timeout(Duration::from_secs(5), l.accept()).await;
            assert!(
                accepted.is_err(),
                "no stream is returned for a bad or missing token"
            );
            let mut buf = [0u8; 1];
            assert_eq!(
                bad.read(&mut buf).await.unwrap_or(0),
                0,
                "the wrong token gets EOF"
            );
            assert_eq!(
                mute.read(&mut buf).await.unwrap_or(0),
                0,
                "the silent one gets EOF"
            );
        });
    }

    #[test]
    fn a_dead_pid_is_reported_stale() {
        let cli = scratch("stale");
        let ep = Endpoint {
            pid: 2147483646,
            port: 1,
            token: "t".into(),
        };
        std::fs::write(cli.join("x.yaml"), serde_yaml::to_string(&ep).unwrap()).unwrap();
        let e = rt().block_on(connect(&cli, "x.yaml")).unwrap_err();
        assert!(e.to_string().contains("stale"), "{e}");
    }

    #[test]
    fn a_good_token_round_trips() {
        let cli = scratch("good").join("cli");
        rt().block_on(async {
            let l = bind(&cli, "x.yaml").unwrap();
            let mut c = connect(&cli, "x.yaml").await.unwrap();
            let mut s = l.accept().await.unwrap();
            c.write_all(b"ping").await.unwrap();
            let mut got = [0u8; 4];
            s.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"ping");
            s.write_all(b"pong").await.unwrap();
            c.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"pong");
        });
    }
}
