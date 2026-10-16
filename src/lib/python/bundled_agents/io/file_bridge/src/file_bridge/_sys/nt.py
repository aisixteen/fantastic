"""Windows: the `0600` contract in Windows terms, through advapi32 (rust
`fantastic_file::sys`'s Windows half — the same rules, the same synthesized bits).

A file is PRIVATE when it is MINE — its owner SID is this process token's user SID
or its default-owner SID (an ELEVATED token, as on CI, stamps
BUILTIN\\Administrators as the owner of what it creates) — and its DACL is
PROTECTED (nothing a parent directory passes down can widen it) with every
ACCESS_ALLOWED ACE naming the user SID. A file created "with mode m",
`m & 0o077 == 0`, gets exactly that: a protected DACL of ONE ACE, full access for
the user SID, set on the new handle before anything is written (the owner stays as
the OS assigns it); any other mode, or none, keeps the ACL the file inherits.

The `mode` and `uid` a refusal reports are synthesized from the judged handle:
`0o600` (`0o400` when read-only) for the owner, `| 0o044` when an ACE grants
anything to anyone else — the owner being the user SID for a file that is mine,
the owner SID otherwise; the uid is the owner SID's RID (its last sub-authority).
"""

from __future__ import annotations

import contextlib
import ctypes
import msvcrt
import ntpath
import os
import stat
from ctypes import wintypes

# defined before `_sys` picks this module, so it is there to import
from file_bridge._sys import Private
from kernel._sys.nt import (
    CREATE_NEW,
    FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES,
    GENERIC_READ,
    GENERIC_WRITE,
    OPEN_EXISTING,
    READ_CONTROL,
    WRITE_DAC,
    create_file,
    kernel32,
    last_error,
)

__all__ = [
    "create_new",
    "is_absolute",
    "is_link",
    "mode_of",
    "open_private",
    "own_uid",
    "set_mode",
]

_advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)

_PVOID = ctypes.c_void_p
_PPVOID = ctypes.POINTER(ctypes.c_void_p)
_advapi32.GetSecurityInfo.argtypes = [
    wintypes.HANDLE,
    ctypes.c_int,
    wintypes.DWORD,
    _PPVOID,
    _PPVOID,
    _PPVOID,
    _PPVOID,
    _PPVOID,
]
_advapi32.GetSecurityInfo.restype = wintypes.DWORD
_advapi32.SetSecurityInfo.argtypes = [
    wintypes.HANDLE,
    ctypes.c_int,
    wintypes.DWORD,
    _PVOID,
    _PVOID,
    _PVOID,
    _PVOID,
]
_advapi32.SetSecurityInfo.restype = wintypes.DWORD
_advapi32.OpenProcessToken.argtypes = [
    wintypes.HANDLE,
    wintypes.DWORD,
    ctypes.POINTER(wintypes.HANDLE),
]
_advapi32.OpenProcessToken.restype = wintypes.BOOL
_advapi32.GetTokenInformation.argtypes = [
    wintypes.HANDLE,
    ctypes.c_int,
    _PVOID,
    wintypes.DWORD,
    ctypes.POINTER(wintypes.DWORD),
]
_advapi32.GetTokenInformation.restype = wintypes.BOOL
_advapi32.EqualSid.argtypes = [_PVOID, _PVOID]
_advapi32.EqualSid.restype = wintypes.BOOL
_advapi32.GetLengthSid.argtypes = [_PVOID]
_advapi32.GetLengthSid.restype = wintypes.DWORD
_advapi32.GetSidSubAuthorityCount.argtypes = [_PVOID]
_advapi32.GetSidSubAuthorityCount.restype = ctypes.POINTER(ctypes.c_ubyte)
_advapi32.GetSidSubAuthority.argtypes = [_PVOID, wintypes.DWORD]
_advapi32.GetSidSubAuthority.restype = ctypes.POINTER(wintypes.DWORD)
_advapi32.GetSecurityDescriptorControl.argtypes = [
    _PVOID,
    ctypes.POINTER(wintypes.WORD),
    ctypes.POINTER(wintypes.DWORD),
]
_advapi32.GetSecurityDescriptorControl.restype = wintypes.BOOL
_advapi32.GetAce.argtypes = [_PVOID, wintypes.DWORD, _PPVOID]
_advapi32.GetAce.restype = wintypes.BOOL
_advapi32.InitializeAcl.argtypes = [_PVOID, wintypes.DWORD, wintypes.DWORD]
_advapi32.InitializeAcl.restype = wintypes.BOOL
_advapi32.AddAccessAllowedAce.argtypes = [
    _PVOID,
    wintypes.DWORD,
    wintypes.DWORD,
    _PVOID,
]
_advapi32.AddAccessAllowedAce.restype = wintypes.BOOL
_advapi32.CreateWellKnownSid.argtypes = [
    ctypes.c_int,
    _PVOID,
    _PVOID,
    ctypes.POINTER(wintypes.DWORD),
]
_advapi32.CreateWellKnownSid.restype = wintypes.BOOL
kernel32.LocalFree.argtypes = [_PVOID]
kernel32.LocalFree.restype = _PVOID
kernel32.GetCurrentProcess.argtypes = []
kernel32.GetCurrentProcess.restype = wintypes.HANDLE

_SE_FILE_OBJECT = 1
_OWNER_SECURITY_INFORMATION = 0x1
_DACL_SECURITY_INFORMATION = 0x4
_PROTECTED_DACL_SECURITY_INFORMATION = 0x80000000
_SE_DACL_PROTECTED = 0x1000
_TOKEN_QUERY = 0x8
_TOKEN_USER = 1  # TOKEN_INFORMATION_CLASS TokenUser
_TOKEN_OWNER = 4  # TokenOwner
_ACL_REVISION = 2
_FILE_ALL_ACCESS = 0x1F01FF
_FILE_GENERIC_READ = 0x120089
_WIN_WORLD_SID = 1  # WELL_KNOWN_SID_TYPE WinWorldSid, S-1-1-0
_SECURITY_MAX_SID_SIZE = 68
_FILE_ATTRIBUTE_READONLY = 0x1
_FILE_ATTRIBUTE_REPARSE_POINT = 0x400
_REPARSE_TAG_NAME_SURROGATE = 0x20000000  # a symlink's tag, a junction's
_ACCESS_ALLOWED_ACE_TYPE = 0x0
# a deny ACE grants nothing
_DENY_ACE_TYPES = frozenset({0x1, 0x6, 0xA, 0xC})
# an ACCESS_ALLOWED_ACE is its 4-byte header and 4-byte mask, then the SID
_ACE_SID_OFFSET = 8


class _ACL(ctypes.Structure):
    _fields_ = [
        ("AclRevision", ctypes.c_ubyte),
        ("Sbz1", ctypes.c_ubyte),
        ("AclSize", wintypes.WORD),
        ("AceCount", wintypes.WORD),
        ("Sbz2", wintypes.WORD),
    ]


class _ACE_HEADER(ctypes.Structure):
    _fields_ = [
        ("AceType", ctypes.c_ubyte),
        ("AceFlags", ctypes.c_ubyte),
        ("AceSize", wintypes.WORD),
    ]


def _fd(h: int, flags: int) -> int:
    """A C-runtime descriptor owning handle `h` (binary — no `O_TEXT`)."""
    try:
        return msvcrt.open_osfhandle(h, flags)
    except BaseException:
        kernel32.CloseHandle(h)
        raise


# ─── create / judge ─────────────────────────────────────────────


def is_absolute(path: str) -> bool:
    """A wire path is absolute when it starts `/` (as on POSIX) — or names a drive
    (`C:`, even drive-relative `C:x`, which would otherwise alias a path under the
    root) or a UNC share: `ntpath.splitdrive` finds one (rust
    `fantastic_bundle::is_absolute`)."""
    return path.startswith("/") or bool(ntpath.splitdrive(path)[0])


def is_link(path) -> bool:
    """Is the entry AT `path` a link (dangling or not) — never followed: a
    NAME-SURROGATE reparse point, a symlink or a junction (rust's
    `FileType::is_symlink`; python 3.11's `is_symlink` misses a junction, and a
    junction followed is a directory whose contents a delete would take)."""
    try:
        st = os.lstat(path)
    except OSError:
        return False
    return bool(
        st.st_file_attributes & _FILE_ATTRIBUTE_REPARSE_POINT
        and st.st_reparse_tag & _REPARSE_TAG_NAME_SURROGATE
    )


def create_new(path, mode: int | None) -> int:
    """Create `path` exclusively for writing — never through a link in its place
    (`FILE_FLAG_OPEN_REPARSE_POINT`: a planted link is an existing name, and
    refused) — and return its descriptor. A `mode` with no group/other bits makes
    it private on its own handle before anything is written; any other mode, or
    none, keeps the ACL it inherits."""
    private = mode is not None and mode & 0o077 == 0
    access = GENERIC_WRITE | (WRITE_DAC if private else 0)
    fd = _fd(create_file(path, access, CREATE_NEW, FILE_FLAG_OPEN_REPARSE_POINT), 0)
    if private:
        try:
            user, _ = _token_sids()
            _set_dacl(msvcrt.get_osfhandle(fd), _acl_for(user, world_read=False))
        except BaseException:
            os.close(fd)
            with contextlib.suppress(FileNotFoundError):
                os.unlink(path)
            raise
    return fd


def open_private(path) -> Private:
    """ONE open that never follows a link in the leaf's place
    (`FILE_FLAG_OPEN_REPARSE_POINT` opens the link itself, judged and refused as
    not a regular file; `FILE_FLAG_BACKUP_SEMANTICS` lets a directory open, to be
    refused the same way), judged by the owner and DACL of THAT handle — so
    nothing can swap the file between the check and the read.
    `FileNotFoundError` when absent."""
    fd = _fd(
        create_file(
            path,
            GENERIC_READ,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
        ),
        os.O_RDONLY,
    )
    try:
        private, mode, uid = _judge(fd)
    except BaseException:
        os.close(fd)
        raise
    if not private:
        os.close(fd)
        return Private(None, mode, uid)
    return Private(fd, mode, uid)


def _judge(fd: int) -> tuple[bool, int, int]:
    """`(private, mode, uid)` of the file open on `fd` (module docs)."""
    st = os.fstat(fd)
    attrs = st.st_file_attributes
    regular = stat.S_ISREG(st.st_mode) and not attrs & _FILE_ATTRIBUTE_REPARSE_POINT
    owner, dacl, descriptor = ctypes.c_void_p(), ctypes.c_void_p(), ctypes.c_void_p()
    rc = _advapi32.GetSecurityInfo(
        msvcrt.get_osfhandle(fd),
        _SE_FILE_OBJECT,
        _OWNER_SECURITY_INFORMATION | _DACL_SECURITY_INFORMATION,
        ctypes.byref(owner),
        None,
        ctypes.byref(dacl),
        None,
        ctypes.byref(descriptor),
    )
    if rc:
        raise last_error(code=rc)
    try:
        user, default_owner = _token_sids()
        mine = bool(owner.value) and bool(
            _advapi32.EqualSid(owner, user) or _advapi32.EqualSid(owner, default_owner)
        )
        others = _grants_beyond(dacl.value, user if mine else owner.value)
        control, revision = wintypes.WORD(), wintypes.DWORD()
        if not _advapi32.GetSecurityDescriptorControl(
            descriptor, ctypes.byref(control), ctypes.byref(revision)
        ):
            raise last_error()
        protected = bool(control.value & _SE_DACL_PROTECTED)
        mode = 0o400 if attrs & _FILE_ATTRIBUTE_READONLY else 0o600
        if others:
            mode |= 0o044
        return regular and mine and protected and not others, mode, _rid(owner.value)
    finally:
        kernel32.LocalFree(descriptor)


def _grants_beyond(dacl, who) -> bool:
    """Does the DACL let anyone but `who` in? A NULL DACL lets everyone in; a deny
    ACE grants nothing; an allow ACE of any kind but the plain ACCESS_ALLOWED one
    is counted as granting someone else."""
    if not dacl:
        return True
    count = ctypes.cast(dacl, ctypes.POINTER(_ACL))[0].AceCount
    for i in range(count):
        ace = ctypes.c_void_p()
        if not _advapi32.GetAce(dacl, i, ctypes.byref(ace)):
            raise last_error()
        kind = ctypes.cast(ace, ctypes.POINTER(_ACE_HEADER))[0].AceType
        if kind in _DENY_ACE_TYPES:
            continue
        if kind != _ACCESS_ALLOWED_ACE_TYPE:
            return True
        if not who or not _advapi32.EqualSid(ace.value + _ACE_SID_OFFSET, who):
            return True
    return False


# ─── SIDs and DACLs ─────────────────────────────────────────────


def _token_sids():
    """This process token's user SID and its default-owner SID, each copied into
    a buffer we own."""
    token = wintypes.HANDLE()
    if not _advapi32.OpenProcessToken(
        kernel32.GetCurrentProcess(), _TOKEN_QUERY, ctypes.byref(token)
    ):
        raise last_error()
    try:
        return _token_sid(token, _TOKEN_USER), _token_sid(token, _TOKEN_OWNER)
    finally:
        kernel32.CloseHandle(token)


def _token_sid(token, cls: int):
    size = wintypes.DWORD()
    _advapi32.GetTokenInformation(token, cls, None, 0, ctypes.byref(size))  # size
    buf = ctypes.create_string_buffer(size.value)
    if not _advapi32.GetTokenInformation(
        token, cls, buf, size.value, ctypes.byref(size)
    ):
        raise last_error()
    # TOKEN_USER and TOKEN_OWNER both start with the pointer to their SID, which
    # lives inside `buf` — copied out before `buf` goes.
    sid = ctypes.cast(buf, ctypes.POINTER(ctypes.c_void_p))[0]
    length = _advapi32.GetLengthSid(sid)
    out = ctypes.create_string_buffer(length)
    ctypes.memmove(out, sid, length)
    return out


def _rid(sid) -> int:
    """The last sub-authority of `sid` — its RID, the "uid" a refusal reports."""
    if not sid:
        return 0
    n = _advapi32.GetSidSubAuthorityCount(sid)[0]
    return _advapi32.GetSidSubAuthority(sid, n - 1)[0] if n else 0


def _world_sid():
    """The Everyone SID (`S-1-1-0`)."""
    size = wintypes.DWORD(_SECURITY_MAX_SID_SIZE)
    buf = ctypes.create_string_buffer(size.value)
    if not _advapi32.CreateWellKnownSid(_WIN_WORLD_SID, None, buf, ctypes.byref(size)):
        raise last_error()
    return buf


def _acl_for(user, *, world_read: bool):
    """A DACL granting `user` full access — and, `world_read`, Everyone read."""
    aces = [(user, _FILE_ALL_ACCESS)]
    if world_read:
        aces.append((_world_sid(), _FILE_GENERIC_READ))
    size = ctypes.sizeof(_ACL) + sum(
        _ACE_SID_OFFSET + _advapi32.GetLengthSid(sid) for sid, _ in aces
    )
    size = (size + 3) & ~3  # an ACL is DWORD-sized
    acl = ctypes.create_string_buffer(size)
    if not _advapi32.InitializeAcl(acl, size, _ACL_REVISION):
        raise last_error()
    for sid, mask in aces:
        if not _advapi32.AddAccessAllowedAce(acl, _ACL_REVISION, mask, sid):
            raise last_error()
    return acl


def _set_dacl(h: int, acl) -> None:
    """Make `acl` the handle's DACL, PROTECTED — nothing inherited is merged in."""
    rc = _advapi32.SetSecurityInfo(
        h,
        _SE_FILE_OBJECT,
        _DACL_SECURITY_INFORMATION | _PROTECTED_DACL_SECURITY_INFORMATION,
        None,
        None,
        acl,
        None,
    )
    if rc:
        raise last_error(code=rc)


# ─── the private-file fixtures (tests) ──────────────────────────


def set_mode(path, mode: int) -> None:
    """ "chmod": a mode with no group/other bits makes `path` private (the DACL a
    private create sets); any other mode grants Everyone read on top — what
    `mode_of` reads back as `0o644`."""
    h = create_file(
        path, READ_CONTROL | WRITE_DAC, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS
    )
    try:
        user, _ = _token_sids()
        _set_dacl(h, _acl_for(user, world_read=bool(mode & 0o077)))
    finally:
        kernel32.CloseHandle(h)


def mode_of(path) -> int:
    """The synthesized permission bits of `path`, followed."""
    fd = _fd(
        create_file(
            path,
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
        ),
        os.O_RDONLY,
    )
    try:
        return _judge(fd)[1]
    finally:
        os.close(fd)


def own_uid() -> int:
    """The uid a file this process creates is owned by, as `open_private`
    reports it: the RID of the token's default owner."""
    _, default_owner = _token_sids()
    return _rid(default_owner)
