//! The Windows side of [`crate::sys`].
//!
//! The `0600` contract in Windows terms. A file is PRIVATE when it is MINE —
//! its owner SID is this process token's user SID or its default-owner SID (an
//! ELEVATED token stamps BUILTIN\Administrators as the owner of what it
//! creates) — and its DACL is PROTECTED (nothing a parent directory passes down
//! can widen it) with every ACCESS_ALLOWED ACE naming the user SID. A file
//! created "with mode m", `m & 0o077 == 0`, gets exactly that: a protected DACL
//! of ONE ACE, full access for the user SID, set on the new handle before
//! anything is written (the owner stays as the OS assigns it).
//!
//! The `mode` and `uid` a refusal reports are synthesized from the judged
//! handle: `0o600` (`0o400` when read-only) for the owner, `| 0o044` when an ACE
//! grants anything to anyone else — the owner being the user SID for a file
//! that is mine, the owner SID otherwise; the uid is the owner SID's RID.

use std::cmp::Ordering;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{addr_of, null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, GENERIC_WRITE, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    AddAccessAllowedAce, CreateWellKnownSid, EqualSid, GetAce, GetLengthSid,
    GetSecurityDescriptorControl, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    InitializeAcl, TokenOwner, TokenUser, WinWorldSid, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
    ACL_REVISION, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_MAX_SID_SIZE,
    SE_DACL_PROTECTED, TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::Private;

/// python's order of two `WindowsPath`s: their LOWERCASED text, part by part
/// (`PurePath._parts_normcase` — Windows paths compare case-insensitively).
pub(crate) fn path_order(a: &Path, b: &Path) -> Ordering {
    let a = a.to_string_lossy().to_lowercase();
    let b = b.to_string_lossy().to_lowercase();
    a.split('\\').cmp(b.split('\\'))
}

/// python `os.unlink` (CPython's `Py_DeleteFileW`): a directory symlink or
/// junction is removed as the directory entry it is (`RemoveDirectoryW`, which
/// leaves its target alone), everything else with `DeleteFileW`.
pub(crate) fn unlink(path: &Path) -> io::Result<()> {
    let dir_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink_dir());
    if dir_link {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// `CreateFileW(CREATE_NEW)` that never goes through a link in the leaf's place
/// (`FILE_FLAG_OPEN_REPARSE_POINT`: a planted link is an existing name, and
/// refused). A mode with no group/other bits makes the new file private on its
/// own handle before anything is written; any other mode, or none, keeps the
/// ACL the file inherits.
pub(crate) fn create_new_nofollow(path: &Path, mode: Option<u32>) -> io::Result<File> {
    let private = mode.is_some_and(|m| m & 0o077 == 0);
    let mut opts = OpenOptions::new();
    opts.write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    if private {
        // the DACL is set through this handle
        opts.access_mode(GENERIC_WRITE | WRITE_DAC);
    }
    let f = opts.open(path)?;
    if private {
        let (user, _) = token_sids()?;
        set_dacl(&f, &acl_for(&user, false)?)?;
    }
    Ok(f)
}

/// ONE open that never follows a link in the leaf's place
/// (`FILE_FLAG_OPEN_REPARSE_POINT` opens the link itself, judged and refused as
/// not a regular file; `FILE_FLAG_BACKUP_SEMANTICS` lets a directory open, to
/// be refused the same way), judged by the owner and DACL of THAT handle.
pub(crate) fn open_private(path: &Path) -> io::Result<Private> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let judged = judge(&f)?;
    Ok(if judged.private {
        Private::File(f)
    } else {
        Private::Not {
            mode: judged.mode,
            uid: judged.uid,
        }
    })
}

/// What [`judge`] makes of an open handle.
struct Judged {
    private: bool,
    mode: u32,
    uid: u32,
}

/// Judge an open handle against the private-file contract (module docs).
fn judge(f: &File) -> io::Result<Judged> {
    let attrs = f.metadata()?.file_attributes();
    let regular = attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) == 0;
    let sec = Security::of(f)?;
    let (user, default_owner) = token_sids()?;
    // SAFETY (both): a non-null owner points into `sec`'s descriptor, alive here;
    // the token SIDs are valid SIDs we own.
    let mine = !sec.owner.is_null()
        && (unsafe { EqualSid(sec.owner, user.ptr()) } != 0
            || unsafe { EqualSid(sec.owner, default_owner.ptr()) } != 0);
    let owner = if mine { user.ptr() } else { sec.owner };
    let others = sec.grants_beyond(owner)?;
    let mut mode = if attrs & FILE_ATTRIBUTE_READONLY != 0 {
        0o400
    } else {
        0o600
    };
    if others {
        mode |= 0o044;
    }
    Ok(Judged {
        private: regular && mine && sec.protected()? && !others,
        mode,
        uid: rid(sec.owner),
    })
}

/// A handle's owner and DACL (`GetSecurityInfo`), released on drop.
struct Security {
    descriptor: PSECURITY_DESCRIPTOR,
    owner: PSID,
    dacl: *mut ACL,
}

impl Security {
    fn of(f: &File) -> io::Result<Self> {
        let mut sec = Security {
            descriptor: null_mut(),
            owner: null_mut(),
            dacl: null_mut(),
        };
        // SAFETY: the handle is owned by `f`, open for this call; the out
        // pointers are valid, and `owner`/`dacl` point into `descriptor`, which
        // `Drop` frees.
        let rc = unsafe {
            GetSecurityInfo(
                f.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut sec.owner,
                null_mut(),
                &mut sec.dacl,
                null_mut(),
                &mut sec.descriptor,
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(sec)
    }

    /// Is the DACL PROTECTED — closed to what a parent directory passes down?
    fn protected(&self) -> io::Result<bool> {
        let (mut control, mut revision) = (0u16, 0u32);
        // SAFETY: `descriptor` is the live descriptor `GetSecurityInfo` gave.
        if unsafe { GetSecurityDescriptorControl(self.descriptor, &mut control, &mut revision) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(control & SE_DACL_PROTECTED != 0)
    }

    /// Does the DACL let anyone but `who` in? A NULL DACL lets everyone in; a
    /// deny ACE grants nothing; an allow ACE of any kind but the plain
    /// ACCESS_ALLOWED one is counted as granting someone else.
    fn grants_beyond(&self, who: PSID) -> io::Result<bool> {
        if self.dacl.is_null() {
            return Ok(true);
        }
        // SAFETY: a non-null `dacl` points into the live descriptor.
        let count = unsafe { (*self.dacl).AceCount };
        for i in 0..u32::from(count) {
            let mut ace: *mut c_void = null_mut();
            // SAFETY: `i` is below the ACL's own ACE count.
            if unsafe { GetAce(self.dacl, i, &mut ace) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: every ACE starts with its header.
            let kind = u32::from(unsafe { (*ace.cast::<ACE_HEADER>()).AceType });
            match kind {
                ACCESS_DENIED_ACE_TYPE
                | ACCESS_DENIED_OBJECT_ACE_TYPE
                | ACCESS_DENIED_CALLBACK_ACE_TYPE
                | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => {}
                ACCESS_ALLOWED_ACE_TYPE => {
                    // SAFETY: an ACCESS_ALLOWED ACE's SID starts at `SidStart`,
                    // inside the ACE; `who` is a valid SID.
                    let sid = unsafe { addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart) };
                    if who.is_null() || unsafe { EqualSid(sid as PSID, who) } == 0 {
                        return Ok(true);
                    }
                }
                _ => return Ok(true),
            }
        }
        Ok(false)
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: `descriptor` was allocated by `GetSecurityInfo` (or is null).
        unsafe { LocalFree(self.descriptor) };
    }
}

/// A SID copied into memory we own, aligned as its 32-bit sub-authorities are.
struct Sid(Vec<u32>);

impl Sid {
    /// Copy the valid SID at `p`.
    fn copy(p: PSID) -> Self {
        // SAFETY: `p` is a valid SID; `GetLengthSid` is its byte length, and the
        // buffer holds at least that many bytes.
        unsafe {
            let len = GetLengthSid(p) as usize;
            let mut buf = vec![0u32; len.div_ceil(4)];
            std::ptr::copy_nonoverlapping(p.cast::<u8>(), buf.as_mut_ptr().cast::<u8>(), len);
            Self(buf)
        }
    }

    fn ptr(&self) -> PSID {
        self.0.as_ptr() as PSID
    }
}

/// This process token's user SID and its default-owner SID.
fn token_sids() -> io::Result<(Sid, Sid)> {
    let mut token: HANDLE = null_mut();
    // SAFETY: the pseudo handle of this process; `token` receives a handle we
    // close below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sids = (|| {
        let user = token_info(token, TokenUser)?;
        let owner = token_info(token, TokenOwner)?;
        // SAFETY: each buffer holds the struct its class names, aligned for it;
        // the SIDs it points at live inside the same buffer.
        Ok(unsafe {
            (
                Sid::copy((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid),
                Sid::copy((*owner.as_ptr().cast::<TOKEN_OWNER>()).Owner),
            )
        })
    })();
    // SAFETY: `token` is the handle opened above, closed once.
    unsafe { CloseHandle(token) };
    sids
}

/// One `GetTokenInformation` class, in a buffer aligned for the struct it holds.
fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let mut len = 0u32;
    // SAFETY: a size query — no buffer, `len` receives the size needed.
    unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut len) };
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes.
    if unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

/// The last sub-authority of `sid` — its RID, the "uid" a refusal reports.
fn rid(sid: PSID) -> u32 {
    if sid.is_null() {
        return 0;
    }
    // SAFETY: `sid` is a valid SID; the index is below its sub-authority count.
    unsafe {
        let n = *GetSidSubAuthorityCount(sid);
        if n == 0 {
            0
        } else {
            *GetSidSubAuthority(sid, u32::from(n) - 1)
        }
    }
}

/// A DACL granting `user` full access — and, `world_read`, Everyone read.
fn acl_for(user: &Sid, world_read: bool) -> io::Result<Vec<u32>> {
    let world = if world_read { Some(world_sid()?) } else { None };
    let mut aces = vec![(user, FILE_ALL_ACCESS)];
    if let Some(w) = &world {
        aces.push((w, FILE_GENERIC_READ));
    }
    // An ACE is its fixed part, less the `SidStart` placeholder, plus its SID.
    let size = size_of::<ACL>()
        + aces
            .iter()
            // SAFETY: each is a valid SID we own.
            .map(|(sid, _)| {
                size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>()
                    + unsafe { GetLengthSid(sid.ptr()) } as usize
            })
            .sum::<usize>();
    let mut buf = vec![0u32; size.div_ceil(4)];
    let acl = buf.as_mut_ptr().cast::<ACL>();
    // SAFETY: `buf` is a writable, aligned buffer of the size passed.
    if unsafe { InitializeAcl(acl, (buf.len() * 4) as u32, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    for (sid, mask) in aces {
        // SAFETY: `acl` was sized for every ACE added here.
        if unsafe { AddAccessAllowedAce(acl, ACL_REVISION, mask, sid.ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(buf)
}

/// The Everyone SID (`S-1-1-0`).
fn world_sid() -> io::Result<Sid> {
    let mut buf = vec![0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];
    let mut len = SECURITY_MAX_SID_SIZE;
    // SAFETY: `buf` holds `len` writable bytes.
    if unsafe { CreateWellKnownSid(WinWorldSid, null_mut(), buf.as_mut_ptr().cast(), &mut len) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Sid(buf))
}

/// Make `acl` the handle's DACL, PROTECTED — nothing inherited is merged in.
fn set_dacl(f: &File, acl: &[u32]) -> io::Result<()> {
    // SAFETY: the handle is owned by `f`, open with WRITE_DAC; `acl` is a valid
    // ACL that outlives the call.
    let rc = unsafe {
        SetSecurityInfo(
            f.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl.as_ptr().cast::<ACL>(),
            null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

// ── the private-file fixtures (`crate::test_support`) ───────────────────────

/// "chmod": a mode with no group/other bits makes `path` private (the DACL a
/// private create sets); any other mode grants Everyone read on top — what
/// [`mode_of`] reads back as `0o644`.
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let f = OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let (user, _) = token_sids()?;
    set_dacl(&f, &acl_for(&user, mode & 0o077 != 0)?)
}

/// The synthesized permission bits of `path`, followed.
pub(crate) fn mode_of(path: &Path) -> io::Result<u32> {
    let f = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    Ok(judge(&f)?.mode)
}

/// The uid a file this process creates is owned by, as [`open_private`]
/// reports it: the RID of the token's default owner.
pub(crate) fn own_uid() -> u32 {
    let (_, default_owner) = token_sids().expect("this process's token is readable");
    rid(default_owner.ptr())
}
