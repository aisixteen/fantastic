//! Windows: `windows-sys` and `std::os::windows`.
//!
//! The workdir lock is a `LockFileEx` byte-range lock. Windows range locks are
//! MANDATORY — a locked byte cannot be read by anyone else — so the lock sits
//! on one byte far past the end of the file, [`LOCK_OFFSET`], and the pid the
//! file holds stays readable to the process that is refused (python reads it
//! to name the holder). Like `flock`, the lock belongs to the open handle and
//! goes when it closes.

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, HANDLE, STILL_ACTIVE,
};
use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, LockFileEx, BY_HANDLE_FILE_INFORMATION, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, LOCK_FILE_FLAGS,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

/// The one byte the workdir lock covers: 2^62, past any file this lock writes.
const LOCK_OFFSET: u64 = 1 << 62;

/// `true` when the process exists, ours or not: it opens and has not exited,
/// or it exists and refuses us the handle.
pub(crate) fn pid_alive(pid: u32) -> bool {
    // SAFETY: a plain query open; a null handle is the failure.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        // SAFETY: reads this thread's last error.
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    let mut code = 0u32;
    // SAFETY: `h` is a live process handle we own; closed right after.
    let alive = unsafe { GetExitCodeProcess(h, &mut code) } != 0 && code == STILL_ACTIVE as u32;
    unsafe { CloseHandle(h) };
    alive
}

/// The permission bits a file created through `opts` gets. Windows files have
/// none (python's `os.open` mode sets only the read-only flag, and every mode
/// passed here is writable): `opts` as it is.
pub(crate) fn set_create_mode(opts: &mut OpenOptions, _mode: u32) -> &mut OpenOptions {
    opts
}

/// Are two open files the same file — same volume, same file index?
pub(crate) fn same_file(a: &File, b: &File) -> std::io::Result<bool> {
    Ok(file_id(a)? == file_id(b)?)
}

fn file_id(f: &File) -> std::io::Result<(u32, u32, u32)> {
    // SAFETY: zeroed is a valid BY_HANDLE_FILE_INFORMATION; the handle is
    // owned by `f`, open for this whole call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

/// Blocking exclusive lock on `file`'s handle.
pub(crate) fn flock_exclusive(file: &File) -> std::io::Result<()> {
    lock(file, LOCKFILE_EXCLUSIVE_LOCK)
}

/// Non-blocking exclusive lock; held elsewhere is
/// [`std::io::ErrorKind::WouldBlock`].
pub(crate) fn try_flock_exclusive(file: &File) -> std::io::Result<()> {
    lock(file, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)
}

/// Non-blocking shared lock; held exclusively elsewhere is
/// [`std::io::ErrorKind::WouldBlock`].
pub(crate) fn try_flock_shared(file: &File) -> std::io::Result<()> {
    lock(file, LOCKFILE_FAIL_IMMEDIATELY)
}

/// `LockFileEx` on the lock byte of `file`'s handle.
fn lock(file: &File, flags: LOCK_FILE_FLAGS) -> std::io::Result<()> {
    // SAFETY: zeroed is a valid OVERLAPPED; only the offset is set.
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.Anonymous.Anonymous.Offset = LOCK_OFFSET as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (LOCK_OFFSET >> 32) as u32;
    // SAFETY: the handle is owned by `file` (opened synchronous, so the call
    // completes before it returns) and `ov` outlives the call.
    if unsafe { LockFileEx(file.as_raw_handle() as HANDLE, flags, 0, 1, 0, &mut ov) } != 0 {
        return Ok(());
    }
    let e = Error::last_os_error();
    if e.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        return Err(Error::from(ErrorKind::WouldBlock));
    }
    Err(e)
}

/// Fill `buf` from the OS CSPRNG (`BCryptGenRandom`, the system RNG). A failure
/// is FATAL: there is no second source.
pub(crate) fn fill_random(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(u32::MAX as usize) {
        // SAFETY: `chunk` is a valid writable buffer of exactly its length; the
        // system-preferred RNG takes no algorithm handle.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        assert!(
            status == 0,
            "the OS randomness call failed: no randomness source"
        );
    }
}

/// A symlink at `link` pointing at `target` — the fixture
/// [`crate::test_support::symlink`] plants. A Windows link is TYPED: a directory
/// link when `target` (a relative one read from the link's own directory) is a
/// directory now, a file link otherwise. A relative target's `/` becomes `\` —
/// the OS reads a link's text with `\` as its only separator.
pub(crate) fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    let target: PathBuf = target.components().collect();
    let base = link.parent().unwrap_or(Path::new(""));
    if base.join(&target).is_dir() {
        std::os::windows::fs::symlink_dir(&target, link)
    } else {
        std::os::windows::fs::symlink_file(&target, link)
    }
}
