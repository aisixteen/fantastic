//! LOGIND's user and session objects, for the agents that act on the kiosk's
//! session. The kiosk user's `Display` session is the one that is in the
//! foreground: the special `session/auto` path would name a closing session
//! after a restart of the kiosk, so the real one is looked up through the
//! caller's user (`GetUserByPID(0)`), re-read at each use.

use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

use crate::sys::linux::os_error;
use crate::sys::OsError;

pub(crate) const LOGIND: &str = "org.freedesktop.login1";

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub(crate) trait LoginManager {
    #[zbus(name = "GetUserByPID")]
    fn get_user_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;
    /// A sleep inhibitor: the lock's fd is closed to let sleep proceed.
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.User",
    default_service = "org.freedesktop.login1"
)]
pub(crate) trait LoginUser {
    /// The id and the object path of the user's display session.
    #[zbus(property)]
    fn display(&self) -> zbus::Result<(String, OwnedObjectPath)>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
pub(crate) trait LoginSession {
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;
    fn set_locked_hint(&self, locked: bool) -> zbus::Result<()>;
    #[zbus(signal)]
    fn lock(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn unlock(&self) -> zbus::Result<()>;
}

/// The object path of the caller's user (`GetUserByPID(0)`: the caller's own).
pub(crate) async fn user_path(conn: &zbus::Connection) -> Result<OwnedObjectPath, OsError> {
    let e = |e| os_error(e, LOGIND);
    let manager = LoginManagerProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(e)?;
    manager.get_user_by_pid(0).await.map_err(e)
}

/// The id and object path of the user's display session now.
pub(crate) async fn display_session(
    conn: &zbus::Connection,
) -> Result<(String, OwnedObjectPath), OsError> {
    let e = |e| os_error(e, LOGIND);
    let user = LoginUserProxy::builder(conn)
        .path(user_path(conn).await?)
        .map_err(e)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(e)?;
    let (id, path) = user.display().await.map_err(e)?;
    if id.is_empty() || path.as_str() == "/" {
        return Err(OsError::Unavailable(
            "logind has no display session for this user".into(),
        ));
    }
    Ok((id, path))
}
