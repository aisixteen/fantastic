//! The Windows side of [`crate::sys`]: the `0600` / `0700` contract as DACLs —
//! the shared rule, as `fantastic-file`'s `sys::windows` keeps it.
//!
//! PRIVATE means a PROTECTED DACL (nothing a parent directory passes down can
//! widen it) whose one ACE grants this process token's user SID full access. A
//! file gets it on its new handle before anything is written; a directory is
//! created with it (`CreateDirectoryW`'s descriptor — no moment it is open to
//! others), its ACE inherited by what is created inside. The owner stays as the
//! OS assigns it (an ELEVATED token stamps BUILTIN\Administrators).
//!
//! The permission bits the `& 0o077` judgement and the refusal text read are
//! SYNTHESIZED from the DACL: `0o600` for a file (`0o400` when read-only),
//! `0o700` for a directory, `| 0o044` when an ACE grants anything to a SID other
//! than the owner — the owner being the user SID for a file that is mine (owned
//! by the token's user or its default owner), the owner SID otherwise.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{addr_of, null, null_mut};

use windows_sys::core::PWSTR;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, GENERIC_WRITE, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetAce, GetLengthSid, GetSecurityDescriptorDacl, GetTokenInformation, TokenOwner,
    TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The variable naming the user's home dir (the first python's
/// `ntpath.expanduser` reads).
pub(crate) const USER_HOME: &str = "USERPROFILE";

/// Create the directory `d` (its parent exists), private from the start.
pub(crate) fn create_private_dir(d: &Path) -> io::Result<()> {
    let (user, _) = token_sids()?;
    let sd = Descriptor::new(&sddl(&user.text()?, true, true, false))?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    let w: Vec<u16> = d.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a NUL-terminated path; `sa` and its descriptor outlive the call.
    if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Create the new file `p` for writing, private before anything is written;
/// refused if it exists.
pub(crate) fn create_private_file(p: &Path) -> io::Result<File> {
    let f = OpenOptions::new()
        .write(true)
        .create_new(true)
        // the DACL is set through this handle
        .access_mode(GENERIC_WRITE | WRITE_DAC)
        .open(p)?;
    let made = token_sids().and_then(|(user, _)| {
        let sd = Descriptor::new(&sddl(&user.text()?, false, true, false))?;
        set_dacl(&f, sd.dacl()?)
    });
    if let Err(e) = made {
        // an empty file that never became private is not left to be reused
        drop(f);
        let _ = std::fs::remove_file(p);
        return Err(e);
    }
    Ok(f)
}

/// The synthesized permission bits of the file at `p`, links followed (module
/// docs).
pub(crate) fn mode_of(p: &Path) -> io::Result<u32> {
    let f = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        // a directory opens only with backup semantics
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(p)?;
    let attrs = f.metadata()?.file_attributes();
    let sec = Security::of(&f)?;
    let (user, default_owner) = token_sids()?;
    // SAFETY (both): a non-null owner points into `sec`'s descriptor, alive here;
    // the token SIDs are valid SIDs we own.
    let mine = !sec.owner.is_null()
        && (unsafe { EqualSid(sec.owner, user.ptr()) } != 0
            || unsafe { EqualSid(sec.owner, default_owner.ptr()) } != 0);
    let owner = if mine { user.ptr() } else { sec.owner };
    let mut mode = if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        0o700
    } else if attrs & FILE_ATTRIBUTE_READONLY != 0 {
        0o400
    } else {
        0o600
    };
    if sec.grants_beyond(owner)? {
        mode |= 0o044;
    }
    Ok(mode)
}

/// The command that makes `p` private again, for a refusal's hint: inheritance
/// off, the user's grant the only one (`icacls`, the user as `cmd` spells it).
pub(crate) fn private_fix(p: &Path, dir: bool) -> String {
    let grant = if dir { "(OI)(CI)F" } else { "F" };
    format!("icacls \"{}\" /inheritance:r /grant:r \"%USERNAME%:{grant}\"", p.display())
}

/// "chmod" for the tests: the user's grant is full access when `mode` has the
/// owner's write bit, read and execute otherwise; a mode with group/other bits
/// grants Everyone read on top (read back as `0o644`). Protected, as a private
/// create makes it.
#[cfg(test)]
pub(crate) fn set_mode(p: &Path, mode: u32) -> io::Result<()> {
    let (user, _) = token_sids()?;
    let sd = Descriptor::new(&sddl(&user.text()?, p.is_dir(), mode & 0o200 != 0, mode & 0o077 != 0))?;
    // the owner may always rewrite the DACL (READ_CONTROL and WRITE_DAC are its
    // own), however little the DACL itself grants
    let f = OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(p)?;
    set_dacl(&f, sd.dacl()?)
}

/// The SDDL of a protected DACL granting `user` full access (`write`) or read
/// and execute, inherited by what a directory holds (`dir`); `world_read` adds
/// Everyone read.
fn sddl(user: &str, dir: bool, write: bool, world_read: bool) -> String {
    let inherit = if dir { "OICI" } else { "" };
    let rights = if write { "FA" } else { "FRFX" };
    let world = if world_read { "(A;;FR;;;WD)" } else { "" };
    format!("D:P(A;{inherit};{rights};;;{user}){world}")
}

/// A security descriptor made from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn new(sddl: &str) -> io::Result<Self> {
        let w: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: a NUL-terminated SDDL string and a valid out-pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(sd))
    }

    /// Its DACL, pointing into the descriptor.
    fn dacl(&self) -> io::Result<*const ACL> {
        let (mut present, mut defaulted) = (0, 0);
        let mut acl: *mut ACL = null_mut();
        // SAFETY: `self.0` is a live descriptor; valid out-pointers.
        if unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut acl, &mut defaulted) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(acl)
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

/// Make `acl` the handle's DACL, PROTECTED — nothing inherited is merged in.
fn set_dacl(f: &File, acl: *const ACL) -> io::Result<()> {
    // SAFETY: the handle is owned by `f`, open with WRITE_DAC; `acl` points into a
    // descriptor that outlives the call.
    let rc = unsafe {
        SetSecurityInfo(
            f.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
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

    /// Its SDDL text (`S-1-5-21-…`).
    fn text(&self) -> io::Result<String> {
        let mut text: PWSTR = null_mut();
        // SAFETY: a valid SID we own; a valid out-pointer.
        if unsafe { ConvertSidToStringSidW(self.ptr(), &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `text` is the NUL-terminated string the call allocated; freed
        // once, after it is copied.
        unsafe {
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Ok(s)
        }
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
