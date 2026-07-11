//! The **generic sub-kernel** — one construct, several roles.
//!
//! A sub-kernel is a `.fantastic/<name>/` namespace + an anchor record (sibling to
//! `web_loader`) + a uniform node in the switcher-vis with a **role** tag and a
//! **live/dormant** status. `self`, `cli`, `web`, a folder-host, and foreigners are
//! all *roles/realizations* of this one shape — `web` is NOT a special case. The
//! only real axis of difference is the [`Role`] (and its [`Realization`]); the
//! config shape + vis are identical.
//!
//! - `cli` = the **initiator** sub-kernel: it launches and drives, and auto-hydrates
//!   its own `<cwd>/.fantastic/cli` natively (it has disk — no browser sandbox, so
//!   no `ProxyLoader`), host-independent.
//! - `web` = **passive/served**: config may exist, but it does NOT auto-hydrate; it
//!   wakes only when a browser loads it. In the CLI it is a config-only *dormant*
//!   node (the host never references `ts/` — weak binding).
//! - folder-host / foreigner / relay = reached over a transport, hydrated on attach.
//!
//! Same config shape, different **realization**: `web` persists remotely through a
//! browser `proxy_loader` over WS; `cli` persists locally through native
//! direct-write (see `fantastic_host::ensure_cli_subkernel`).

use std::path::PathBuf;

use fantastic_host::gateway::Runtime;

/// What a sub-kernel *is* in the federation — the only real axis of difference
/// between instances (the config shape + vis are identical across roles).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// The in-proc manager kernel itself (`★ self`) — always live, no config dir.
    SelfManager,
    /// The **initiator** sub-kernel (`cli`): native, folder-anchored, auto-hydrated.
    Initiator,
    /// A **passive/served** sub-kernel (`web`): config-only until a browser loads it.
    Passive,
    /// The optional per-folder **host** kernel (singleton), reached over a bridge.
    FolderHost,
    /// A **foreigner**: a kernel we attached to (someone else spawned it).
    Foreign,
    /// A **relay** peer, reached through a relay directory (roadmap).
    // NOTE(subkernel-roadmap): relay-directory nodes (`list_peers`) are `26_federation`.
    #[allow(dead_code)]
    Relay,
}

impl Role {
    /// The short tag shown on the node's second line.
    pub fn tag(&self) -> &'static str {
        match self {
            Role::SelfManager => "in-proc",
            Role::Initiator => "initiator",
            Role::Passive => "served",
            Role::FolderHost => "folder",
            Role::Foreign => "foreign",
            Role::Relay => "relay",
        }
    }

    /// True for the roles the in-proc manager kernel drives directly (no transport):
    /// `self` and the initiator `cli` (its folder-anchored namespace projects the
    /// same in-proc kernel).
    pub fn is_native(&self) -> bool {
        matches!(self, Role::SelfManager | Role::Initiator)
    }
}

/// *How* a sub-kernel's live kernel is realized when hydrated — the "different
/// realization" half of "same config shape, different realization".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Realization {
    /// The in-proc manager kernel (`self`).
    InProc,
    /// Native local disk hydration/persistence (`cli` — no loader agent, no proxy).
    Native,
    /// A browser kernel hydrating over a WS `proxy_loader` (`web`).
    BrowserProxy,
    /// Reached over the `ws_bridge` connector (folder-host / foreigner).
    WsBridge,
    /// Reached over the `relay_connector` directory (roadmap).
    // NOTE(subkernel-roadmap): relay transport is `26_federation`.
    #[allow(dead_code)]
    RelayConnector,
}

/// Whether a sub-kernel's live kernel is currently running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Hydrated — the live kernel is running (`● live`).
    Live,
    /// Configured but not opened — the on-disk config exists, no live kernel
    /// (`○ dormant`). Lazy hydration means this is the resting state.
    Dormant,
}

impl Status {
    /// The status glyph shown on the node (`●` live / `○` dormant).
    pub fn glyph(&self) -> char {
        match self {
            Status::Live => '●',
            Status::Dormant => '○',
        }
    }
}

/// One sub-kernel node — its identity + role + vis state. This is the uniform model
/// the switcher renders (`self`, `cli`, folder-host, foreigners all as one shape),
/// and the descriptor the active-kernel Remote arm carries.
#[derive(Clone)]
pub struct SubKernel {
    /// Display name: `self`, `cli`, a folder basename, or a peer label.
    pub name: String,
    pub role: Role,
    // NOTE(subkernel-roadmap): `realization` + `config_path` are the descriptor half
    // of the model (recorded now, consumed when lazy hydration routes per-realization
    // and the switcher folds into the Hub connector list — `26_federation`).
    #[allow(dead_code)]
    pub realization: Realization,
    /// The `.fantastic/<name>` namespace (config-anchored roles: `cli`, `web`).
    #[allow(dead_code)]
    pub config_path: Option<PathBuf>,
    /// The launch dir the gateway attaches/spawns in (folder-host / foreigner).
    pub dir: Option<PathBuf>,
    /// The runtime to spawn a folder-host / foreigner with.
    pub runtime: Option<Runtime>,
    pub status: Status,
    /// The kernel-instance guid (dedup key across restarts + direct↔relay), if known.
    pub guid: Option<String>,
}

impl SubKernel {
    /// The `★ self` in-proc manager node — always first, always live.
    pub fn self_manager() -> SubKernel {
        SubKernel {
            name: "self".into(),
            role: Role::SelfManager,
            realization: Realization::InProc,
            config_path: None,
            dir: None,
            runtime: None,
            status: Status::Live,
            guid: None,
        }
    }

    /// The `cli` initiator node, anchored at `<cwd>/.fantastic/cli` (auto-hydrated).
    pub fn cli(config_path: PathBuf) -> SubKernel {
        SubKernel {
            name: "cli".into(),
            role: Role::Initiator,
            realization: Realization::Native,
            config_path: Some(config_path),
            dir: None,
            runtime: None,
            status: Status::Live,
            guid: None,
        }
    }

    /// The `web` **passive/served** node — config-only, always `○ dormant` in the CLI
    /// (it hydrates only when a browser loads it — out of CLI scope, weak binding).
    /// This is what makes `web` just another sub-kernel on the plane, not a special
    /// case. Surfaced when `<cwd>/.fantastic/web` exists.
    pub fn web(config_path: PathBuf) -> SubKernel {
        SubKernel {
            name: "web".into(),
            role: Role::Passive,
            realization: Realization::BrowserProxy,
            config_path: Some(config_path),
            dir: None,
            runtime: None,
            status: Status::Dormant,
            guid: None,
        }
    }

    /// A folder-host / foreigner node for `dir` (Live if a reflect-ping answered).
    pub fn folder(dir: PathBuf, runtime: Runtime, role: Role, live: bool) -> SubKernel {
        SubKernel {
            name: dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned()),
            role,
            realization: Realization::WsBridge,
            config_path: Some(dir.join(".fantastic")),
            dir: Some(dir),
            runtime: Some(runtime),
            status: if live { Status::Live } else { Status::Dormant },
            guid: None,
        }
    }

    /// The node's second/third vis lines: `(role tag, status word)`. `active`
    /// overrides the status word with `active` (this is the kernel now driving).
    pub fn vis_lines(&self, active: bool) -> (String, String) {
        let tag = match self.runtime {
            Some(rt) => format!("{} · {}", self.role.tag(), rt.as_str()),
            None => self.role.tag().to_string(),
        };
        let status = if active {
            "active".to_string()
        } else {
            match self.status {
                Status::Live => format!("{} live", Status::Live.glyph()),
                Status::Dormant => format!("{} dormant", Status::Dormant.glyph()),
            }
        };
        (tag, status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_tag_and_native() {
        assert_eq!(Role::SelfManager.tag(), "in-proc");
        assert_eq!(Role::Initiator.tag(), "initiator");
        assert!(Role::SelfManager.is_native());
        assert!(Role::Initiator.is_native());
        assert!(!Role::FolderHost.is_native());
        assert!(!Role::Foreign.is_native());
    }

    #[test]
    fn status_glyphs() {
        assert_eq!(Status::Live.glyph(), '●');
        assert_eq!(Status::Dormant.glyph(), '○');
    }

    #[test]
    fn self_and_cli_are_live_native() {
        let s = SubKernel::self_manager();
        assert_eq!(s.role, Role::SelfManager);
        assert_eq!(s.status, Status::Live);
        assert!(s.config_path.is_none());

        let c = SubKernel::cli(PathBuf::from("/proj/.fantastic/cli"));
        assert_eq!(c.role, Role::Initiator);
        assert_eq!(c.realization, Realization::Native);
        assert_eq!(
            c.config_path.as_deref(),
            Some(std::path::Path::new("/proj/.fantastic/cli"))
        );
        assert_eq!(c.status, Status::Live);
    }

    #[test]
    fn web_node_is_passive_dormant() {
        let w = SubKernel::web(PathBuf::from("/proj/.fantastic/web"));
        assert_eq!(w.name, "web");
        assert_eq!(w.role, Role::Passive);
        assert_eq!(w.realization, Realization::BrowserProxy);
        assert_eq!(w.status, Status::Dormant);
        assert!(w.dir.is_none()); // not gateway-attachable; a browser hydrates it
        let (tag, status) = w.vis_lines(false);
        assert_eq!(tag, "served");
        assert_eq!(status, "○ dormant");
    }

    #[test]
    fn folder_node_status_from_liveness() {
        let dir = PathBuf::from("/proj/foo");
        let live = SubKernel::folder(dir.clone(), Runtime::Rust, Role::FolderHost, true);
        assert_eq!(live.name, "foo");
        assert_eq!(live.role, Role::FolderHost);
        assert_eq!(live.status, Status::Live);
        assert_eq!(
            live.config_path.as_deref(),
            Some(std::path::Path::new("/proj/foo/.fantastic"))
        );

        let dormant = SubKernel::folder(dir, Runtime::Rust, Role::Foreign, false);
        assert_eq!(dormant.role, Role::Foreign);
        assert_eq!(dormant.status, Status::Dormant);
    }

    #[test]
    fn vis_lines_active_overrides_status() {
        let s = SubKernel::self_manager();
        let (tag, status) = s.vis_lines(true);
        assert_eq!(tag, "in-proc");
        assert_eq!(status, "active");

        let f = SubKernel::folder(
            PathBuf::from("/p/bar"),
            Runtime::Python,
            Role::FolderHost,
            true,
        );
        let (tag, status) = f.vis_lines(false);
        assert_eq!(tag, "folder · python");
        assert_eq!(status, "● live");
    }
}
