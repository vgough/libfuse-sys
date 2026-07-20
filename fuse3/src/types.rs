//! Safe data types for the `fuse3` low-level API.
//!
//! Everything here is built out of the raw bindings in
//! `libfuse_sys::fuse_lowlevel`, but the public surface uses only `std`
//! types. Per-OS differences (mode_t width, `stat` timestamp field names,
//! Darwin's extra `st_flags`/`st_birthtimespec`, ...) are handled internally
//! by the `to_*`/`from_*` conversions and never leak out.

// Many casts below (e.g. `uid_t` -> `u32`, `tv_sec` -> `i64`) are only
// "unnecessary" on the specific host/OS libfuse-sys happened to be built
// for; the raw field widths differ across macOS/Linux, so the casts stay
// explicit for portability rather than relying on whatever happens to be a
// no-op here.
#![allow(clippy::unnecessary_cast)]

use libfuse_sys::fuse_lowlevel::{
    fuse_conn_info, fuse_ctx, fuse_entry_param, fuse_file_info, fuse_req_ctx, fuse_req_interrupted,
    fuse_req_t, stat, FUSE_SET_ATTR_ATIME, FUSE_SET_ATTR_ATIME_NOW, FUSE_SET_ATTR_GID,
    FUSE_SET_ATTR_MODE, FUSE_SET_ATTR_MTIME, FUSE_SET_ATTR_MTIME_NOW, FUSE_SET_ATTR_SIZE,
    FUSE_SET_ATTR_UID,
};
#[cfg(target_os = "macos")]
use libfuse_sys::fuse_lowlevel::{statfs, timespec, FUSE_SET_ATTR_BTIME, FUSE_SET_ATTR_CTIME};
#[cfg(not(target_os = "macos"))]
use libfuse_sys::fuse_lowlevel::{statvfs, FUSE_SET_ATTR_CTIME};

use std::ffi::CString;
use std::marker::PhantomData;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use crate::darwin::{fuse_add_direntry_plus_vanilla, fuse_add_direntry_vanilla};
#[cfg(not(target_os = "macos"))]
use libfuse_sys::fuse_lowlevel::{fuse_add_direntry, fuse_add_direntry_plus};

/// The fixed FUSE inode number type.
pub type Inode = u64;

/// The inode number of the filesystem root (`FUSE_ROOT_ID`).
pub const ROOT_INODE: Inode = 1;

#[cfg(target_os = "macos")]
fn raw_add_direntry(
    req: fuse_req_t,
    buf: *mut c_char,
    bufsize: usize,
    name: *const c_char,
    stbuf: *const stat,
    off: i64,
) -> usize {
    unsafe { fuse_add_direntry_vanilla(req, buf, bufsize, name, stbuf, off) }
}
#[cfg(not(target_os = "macos"))]
fn raw_add_direntry(
    req: fuse_req_t,
    buf: *mut c_char,
    bufsize: usize,
    name: *const c_char,
    stbuf: *const stat,
    off: i64,
) -> usize {
    unsafe { fuse_add_direntry(req, buf, bufsize, name, stbuf, off) }
}

#[cfg(target_os = "macos")]
fn raw_add_direntry_plus(
    req: fuse_req_t,
    buf: *mut c_char,
    bufsize: usize,
    name: *const c_char,
    e: *const fuse_entry_param,
    off: i64,
) -> usize {
    unsafe { fuse_add_direntry_plus_vanilla(req, buf, bufsize, name, e, off) }
}
#[cfg(not(target_os = "macos"))]
fn raw_add_direntry_plus(
    req: fuse_req_t,
    buf: *mut c_char,
    bufsize: usize,
    name: *const c_char,
    e: *const fuse_entry_param,
    off: i64,
) -> usize {
    unsafe { fuse_add_direntry_plus(req, buf, bufsize, name, e, off) }
}

// ---------------------------------------------------------------------
// Errno
// ---------------------------------------------------------------------

/// A POSIX error number, returned by fallible `Filesystem` trait methods.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Errno(i32);

impl Errno {
    pub const EPERM: Errno = Errno(libc::EPERM);
    pub const ENOENT: Errno = Errno(libc::ENOENT);
    pub const EIO: Errno = Errno(libc::EIO);
    pub const EACCES: Errno = Errno(libc::EACCES);
    pub const EEXIST: Errno = Errno(libc::EEXIST);
    pub const ENOTDIR: Errno = Errno(libc::ENOTDIR);
    pub const EISDIR: Errno = Errno(libc::EISDIR);
    pub const EINVAL: Errno = Errno(libc::EINVAL);
    pub const ENOSYS: Errno = Errno(libc::ENOSYS);
    pub const ENOTEMPTY: Errno = Errno(libc::ENOTEMPTY);
    pub const ERANGE: Errno = Errno(libc::ERANGE);
    /// "No data available" - used for missing extended attributes.
    pub const ENODATA: Errno = Errno(libc::ENODATA);
    /// Alias of [`Errno::ENODATA`] (the BSD/macOS name for the same error).
    pub const ENOATTR: Errno = Errno(libc::ENOATTR);
    pub const EILSEQ: Errno = Errno(libc::EILSEQ);
    pub const ENOSPC: Errno = Errno(libc::ENOSPC);
    pub const EROFS: Errno = Errno(libc::EROFS);
    pub const EBADF: Errno = Errno(libc::EBADF);
    pub const ENAMETOOLONG: Errno = Errno(libc::ENAMETOOLONG);
    pub const ENXIO: Errno = Errno(libc::ENXIO);
    pub const EOPNOTSUPP: Errno = Errno(libc::EOPNOTSUPP);

    /// Wraps a raw `errno` value.
    pub const fn from_raw(errno: i32) -> Self {
        Errno(errno)
    }

    /// Returns the raw `errno` value.
    pub fn raw(self) -> i32 {
        self.0
    }
}

impl From<i32> for Errno {
    fn from(value: i32) -> Self {
        Errno(value)
    }
}

impl From<std::io::Error> for Errno {
    fn from(err: std::io::Error) -> Self {
        Errno(err.raw_os_error().unwrap_or(libc::EIO))
    }
}

impl std::fmt::Display for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "errno {}", self.0)
    }
}

// ---------------------------------------------------------------------
// FileType
// ---------------------------------------------------------------------

/// The type of a filesystem entry, corresponding to the `S_IFMT` bits of
/// `st_mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum FileType {
    #[default]
    RegularFile,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    NamedPipe,
    Socket,
}

impl FileType {
    /// Returns the `S_IFMT` bits corresponding to this file type.
    pub(crate) fn to_mode_bits(self) -> u32 {
        (match self {
            FileType::RegularFile => libc::S_IFREG,
            FileType::Directory => libc::S_IFDIR,
            FileType::Symlink => libc::S_IFLNK,
            FileType::CharDevice => libc::S_IFCHR,
            FileType::BlockDevice => libc::S_IFBLK,
            FileType::NamedPipe => libc::S_IFIFO,
            FileType::Socket => libc::S_IFSOCK,
        }) as u32
    }

    /// Decodes a file type from the `S_IFMT` bits of a mode value.
    ///
    /// Currently only exercised by this module's round-trip tests; kept
    /// alongside [`FileType::to_mode_bits`] as the natural inverse.
    #[allow(dead_code)]
    pub(crate) fn from_mode_bits(mode: u32) -> Option<Self> {
        match mode & (libc::S_IFMT as u32) {
            m if m == libc::S_IFREG as u32 => Some(FileType::RegularFile),
            m if m == libc::S_IFDIR as u32 => Some(FileType::Directory),
            m if m == libc::S_IFLNK as u32 => Some(FileType::Symlink),
            m if m == libc::S_IFCHR as u32 => Some(FileType::CharDevice),
            m if m == libc::S_IFBLK as u32 => Some(FileType::BlockDevice),
            m if m == libc::S_IFIFO as u32 => Some(FileType::NamedPipe),
            m if m == libc::S_IFSOCK as u32 => Some(FileType::Socket),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------
// time helpers
// ---------------------------------------------------------------------

/// Splits a `SystemTime` into `(seconds, nanoseconds)` relative to the Unix
/// epoch, gracefully handling times before the epoch (negative seconds).
fn system_time_to_secs_nsecs(t: SystemTime) -> (i64, i64) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos() as i64),
        Err(e) => {
            let d = e.duration();
            let secs = d.as_secs() as i64;
            let nsecs = d.subsec_nanos() as i64;
            if nsecs == 0 {
                (-secs, 0)
            } else {
                (-secs - 1, 1_000_000_000 - nsecs)
            }
        }
    }
}

/// Inverse of [`system_time_to_secs_nsecs`].
fn secs_nsecs_to_system_time(secs: i64, nsecs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nsecs as u32)
    } else {
        UNIX_EPOCH - Duration::new((-secs) as u64, 0) + Duration::new(0, nsecs as u32)
    }
}

// ---------------------------------------------------------------------
// FileAttr
// ---------------------------------------------------------------------

/// Filesystem entry attributes (the safe equivalent of `struct stat`).
#[derive(Clone, Copy, Debug)]
pub struct FileAttr {
    pub ino: Inode,
    pub size: u64,
    pub blocks: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    /// Creation time. Only meaningful on macOS; ignored on Linux.
    pub crtime: SystemTime,
    pub kind: FileType,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    /// BSD file flags (`chflags(2)`). Only meaningful on macOS; ignored on
    /// Linux.
    pub flags: u32,
}

impl Default for FileAttr {
    fn default() -> Self {
        FileAttr {
            ino: 0,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::default(),
            perm: 0,
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 0,
            flags: 0,
        }
    }
}

impl FileAttr {
    /// Builds a zeroed raw `stat` struct from this attribute set, handling
    /// all per-OS layout differences.
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn to_stat(&self) -> stat {
        let mut st: stat = unsafe { std::mem::zeroed() };

        st.st_ino = self.ino as _;
        st.st_mode = (self.kind.to_mode_bits() | (self.perm as u32 & 0o7777)) as _;
        st.st_nlink = self.nlink as _;
        st.st_uid = self.uid as _;
        st.st_gid = self.gid as _;
        st.st_rdev = self.rdev as _;
        st.st_size = self.size as _;
        st.st_blocks = self.blocks as _;
        st.st_blksize = self.blksize as _;

        let (atime_sec, atime_nsec) = system_time_to_secs_nsecs(self.atime);
        let (mtime_sec, mtime_nsec) = system_time_to_secs_nsecs(self.mtime);
        let (ctime_sec, ctime_nsec) = system_time_to_secs_nsecs(self.ctime);

        #[cfg(target_os = "macos")]
        {
            let (crtime_sec, crtime_nsec) = system_time_to_secs_nsecs(self.crtime);
            st.st_atimespec = timespec {
                tv_sec: atime_sec as _,
                tv_nsec: atime_nsec as _,
            };
            st.st_mtimespec = timespec {
                tv_sec: mtime_sec as _,
                tv_nsec: mtime_nsec as _,
            };
            st.st_ctimespec = timespec {
                tv_sec: ctime_sec as _,
                tv_nsec: ctime_nsec as _,
            };
            st.st_birthtimespec = timespec {
                tv_sec: crtime_sec as _,
                tv_nsec: crtime_nsec as _,
            };
            st.st_flags = self.flags;
        }
        #[cfg(not(target_os = "macos"))]
        {
            st.st_atime = atime_sec as _;
            st.st_atime_nsec = atime_nsec as _;
            st.st_mtime = mtime_sec as _;
            st.st_mtime_nsec = mtime_nsec as _;
            st.st_ctime = ctime_sec as _;
            st.st_ctime_nsec = ctime_nsec as _;
            // crtime/flags have no vanilla `stat` field on Linux.
        }

        st
    }
}

// ---------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------

/// The reply to a `lookup`/`mknod`/`mkdir`/... call: an inode plus caching
/// hints.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub ino: Inode,
    pub generation: u64,
    pub attr: FileAttr,
    pub attr_timeout: Duration,
    pub entry_timeout: Duration,
}

impl Entry {
    pub(crate) fn to_entry_param(&self) -> fuse_entry_param {
        fuse_entry_param {
            ino: self.ino,
            generation: self.generation,
            attr: self.attr.to_stat(),
            attr_timeout: self.attr_timeout.as_secs_f64(),
            entry_timeout: self.entry_timeout.as_secs_f64(),
        }
    }
}

// ---------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------

/// A borrowed view of an in-flight FUSE request, exposing the calling
/// process's credentials.
pub struct Request<'a> {
    req: fuse_req_t,
    _marker: PhantomData<&'a ()>,
}

impl<'a> Request<'a> {
    pub(crate) fn new(req: fuse_req_t) -> Self {
        Request {
            req,
            _marker: PhantomData,
        }
    }

    /// The underlying raw request handle. Not currently needed by any
    /// trampoline (each shim already has `req` in scope directly), kept
    /// for API completeness.
    #[allow(dead_code)]
    pub(crate) fn raw(&self) -> fuse_req_t {
        self.req
    }

    fn ctx(&self) -> &fuse_ctx {
        unsafe { &*fuse_req_ctx(self.req) }
    }

    /// User ID of the calling process.
    pub fn uid(&self) -> u32 {
        self.ctx().uid as u32
    }

    /// Group ID of the calling process.
    pub fn gid(&self) -> u32 {
        self.ctx().gid as u32
    }

    /// Process ID (thread group ID) of the calling process.
    pub fn pid(&self) -> u32 {
        self.ctx().pid as u32
    }

    /// Umask of the calling process.
    pub fn umask(&self) -> u32 {
        self.ctx().umask as u32
    }

    /// Whether the kernel has requested that this operation be interrupted.
    pub fn interrupted(&self) -> bool {
        unsafe { fuse_req_interrupted(self.req) != 0 }
    }
}

// ---------------------------------------------------------------------
// FileInfo
// ---------------------------------------------------------------------

/// A safe, read-only view of an incoming `fuse_file_info`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileInfo {
    pub flags: i32,
    pub fh: u64,
    pub lock_owner: u64,
    pub flush: bool,
    pub writepage: bool,
}

impl FileInfo {
    /// Builds a `FileInfo` from a raw pointer, returning `None` if it is
    /// null (as it legitimately is for several callbacks).
    pub(crate) fn from_raw(fi: *mut fuse_file_info) -> Option<FileInfo> {
        if fi.is_null() {
            return None;
        }
        let fi = unsafe { &*fi };
        Some(FileInfo {
            flags: fi.flags,
            fh: fi.fh,
            lock_owner: fi.lock_owner,
            flush: fi.flush() != 0,
            writepage: fi.writepage() != 0,
        })
    }

    /// The access mode requested in the open flags.
    pub fn access_mode(&self) -> AccessMode {
        match self.flags & libc::O_ACCMODE {
            libc::O_RDONLY => AccessMode::ReadOnly,
            libc::O_WRONLY => AccessMode::WriteOnly,
            _ => AccessMode::ReadWrite,
        }
    }
}

/// The access mode of an open request, decoded from the open flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessMode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

// ---------------------------------------------------------------------
// OpenReply
// ---------------------------------------------------------------------

/// The reply to `open`/`create`/`opendir`: a file handle plus caching hints
/// written back into the kernel's `fuse_file_info`.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenReply {
    pub fh: u64,
    direct_io: bool,
    keep_cache: bool,
    nonseekable: bool,
    cache_readdir: bool,
}

impl OpenReply {
    /// Creates a reply carrying the given file handle, with all caching
    /// hints left at their default (off).
    pub fn new(fh: u64) -> Self {
        OpenReply {
            fh,
            ..Default::default()
        }
    }

    pub fn direct_io(mut self, value: bool) -> Self {
        self.direct_io = value;
        self
    }

    pub fn keep_cache(mut self, value: bool) -> Self {
        self.keep_cache = value;
        self
    }

    pub fn nonseekable(mut self, value: bool) -> Self {
        self.nonseekable = value;
        self
    }

    pub fn cache_readdir(mut self, value: bool) -> Self {
        self.cache_readdir = value;
        self
    }

    /// Writes this reply's fields into the raw `fuse_file_info`. No-op if
    /// `fi` is null.
    pub(crate) fn apply(&self, fi: *mut fuse_file_info) {
        if fi.is_null() {
            return;
        }
        unsafe {
            (*fi).fh = self.fh;
            (*fi).set_direct_io(self.direct_io as u32);
            (*fi).set_keep_cache(self.keep_cache as u32);
            (*fi).set_nonseekable(self.nonseekable as u32);
            (*fi).set_cache_readdir(self.cache_readdir as u32);
        }
    }
}

// ---------------------------------------------------------------------
// SetAttrs
// ---------------------------------------------------------------------

/// Either a specific point in time, or "now" (as requested by e.g.
/// `utimes(2)` with `UTIME_NOW`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TimeOrNow {
    SpecificTime(SystemTime),
    Now,
}

/// The decoded `setattr` request: which fields the caller wants changed,
/// and to what.
#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttrs {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<TimeOrNow>,
    pub mtime: Option<TimeOrNow>,
    pub ctime: Option<SystemTime>,
    /// Creation time (macOS only; the `FUSE_SET_ATTR_BTIME` bit).
    pub crtime: Option<SystemTime>,
    /// "Change time" (no corresponding bit/field is exposed by the vanilla
    /// `stat` layout this crate uses; always `None`).
    pub chgtime: Option<SystemTime>,
    /// Backup time (macOS only; the `FUSE_SET_ATTR_BKUPTIME` bit exists but
    /// the vanilla `stat` layout has no field to source it from, so this is
    /// always `None` for now).
    pub bkuptime: Option<SystemTime>,
    /// BSD file flags (macOS only; the `FUSE_SET_ATTR_FLAGS` bit, sourced
    /// from `st_flags`).
    pub flags: Option<u32>,
}

impl SetAttrs {
    /// Decodes a `setattr` request from the raw `stat`/`to_set` bitmask
    /// pair.
    pub(crate) fn from_raw(attr: *const stat, to_set: c_int) -> SetAttrs {
        let to_set = to_set as u32;
        let mut out = SetAttrs::default();

        if attr.is_null() {
            return out;
        }
        let st = unsafe { &*attr };

        if to_set & FUSE_SET_ATTR_MODE != 0 {
            out.mode = Some(st.st_mode as u32);
        }
        if to_set & FUSE_SET_ATTR_UID != 0 {
            out.uid = Some(st.st_uid as u32);
        }
        if to_set & FUSE_SET_ATTR_GID != 0 {
            out.gid = Some(st.st_gid as u32);
        }
        if to_set & FUSE_SET_ATTR_SIZE != 0 {
            out.size = Some(st.st_size as u64);
        }

        #[cfg(target_os = "macos")]
        {
            if to_set & FUSE_SET_ATTR_ATIME_NOW != 0 {
                out.atime = Some(TimeOrNow::Now);
            } else if to_set & FUSE_SET_ATTR_ATIME != 0 {
                out.atime = Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                    st.st_atimespec.tv_sec as i64,
                    st.st_atimespec.tv_nsec as i64,
                )));
            }
            if to_set & FUSE_SET_ATTR_MTIME_NOW != 0 {
                out.mtime = Some(TimeOrNow::Now);
            } else if to_set & FUSE_SET_ATTR_MTIME != 0 {
                out.mtime = Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                    st.st_mtimespec.tv_sec as i64,
                    st.st_mtimespec.tv_nsec as i64,
                )));
            }
            if to_set & FUSE_SET_ATTR_CTIME != 0 {
                out.ctime = Some(secs_nsecs_to_system_time(
                    st.st_ctimespec.tv_sec as i64,
                    st.st_ctimespec.tv_nsec as i64,
                ));
            }
            if to_set & FUSE_SET_ATTR_BTIME != 0 {
                out.crtime = Some(secs_nsecs_to_system_time(
                    st.st_birthtimespec.tv_sec as i64,
                    st.st_birthtimespec.tv_nsec as i64,
                ));
            }
            // FUSE_SET_ATTR_BKUPTIME exists as a constant, but the vanilla
            // `stat` layout has no distinct field to source it from, so
            // `bkuptime` is never populated.
            if to_set & libfuse_sys::fuse_lowlevel::FUSE_SET_ATTR_FLAGS != 0 {
                out.flags = Some(st.st_flags);
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            if to_set & FUSE_SET_ATTR_ATIME_NOW != 0 {
                out.atime = Some(TimeOrNow::Now);
            } else if to_set & FUSE_SET_ATTR_ATIME != 0 {
                out.atime = Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                    st.st_atime as i64,
                    st.st_atime_nsec as i64,
                )));
            }
            if to_set & FUSE_SET_ATTR_MTIME_NOW != 0 {
                out.mtime = Some(TimeOrNow::Now);
            } else if to_set & FUSE_SET_ATTR_MTIME != 0 {
                out.mtime = Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                    st.st_mtime as i64,
                    st.st_mtime_nsec as i64,
                )));
            }
            if to_set & FUSE_SET_ATTR_CTIME != 0 {
                out.ctime = Some(secs_nsecs_to_system_time(
                    st.st_ctime as i64,
                    st.st_ctime_nsec as i64,
                ));
            }
            // crtime/bkuptime/flags have no vanilla `stat` field on Linux.
        }

        out
    }
}

// ---------------------------------------------------------------------
// StatFs
// ---------------------------------------------------------------------

/// Filesystem-wide statistics, the safe equivalent of `statvfs`/`statfs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatFs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
}

impl StatFs {
    /// Converts to the raw type expected by `fuse_reply_statfs` on this
    /// platform (`statfs` on macOS, `statvfs` on Linux).
    #[allow(clippy::wrong_self_convention)]
    #[cfg(target_os = "macos")]
    pub(crate) fn to_raw(&self) -> statfs {
        let mut out: statfs = unsafe { std::mem::zeroed() };
        out.f_bsize = self.bsize;
        out.f_blocks = self.blocks;
        out.f_bfree = self.bfree;
        out.f_bavail = self.bavail;
        out.f_files = self.files;
        out.f_ffree = self.ffree;
        // macOS's `statfs` has no filename-length-limit field; `f_iosize`
        // (preferred I/O size) is the closest analog to `frsize`.
        out.f_iosize = self.frsize as i32;
        out
    }

    #[allow(clippy::wrong_self_convention)]
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn to_raw(&self) -> statvfs {
        let mut out: statvfs = unsafe { std::mem::zeroed() };
        out.f_bsize = self.bsize as _;
        out.f_frsize = self.frsize as _;
        out.f_blocks = self.blocks as _;
        out.f_bfree = self.bfree as _;
        out.f_bavail = self.bavail as _;
        out.f_files = self.files as _;
        out.f_ffree = self.ffree as _;
        out.f_namemax = self.namelen as _;
        out
    }
}

// ---------------------------------------------------------------------
// DirBuffer / DirPlusBuffer
// ---------------------------------------------------------------------

/// The wire format [`DirBuffer`] emits entries in. See
/// [`DirBuffer::new_plus_fallback`] for why a `readdir`-shaped buffer ever
/// needs to speak the READDIRPLUS format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DirBufferFormat {
    /// Plain `READDIR` entries via `fuse_add_direntry` (the normal case).
    Plain,
    /// `READDIRPLUS`-shaped entries via `fuse_add_direntry_plus`, with a
    /// synthesized, non-caching `fuse_entry_param` per entry. See
    /// [`DirBuffer::new_plus_fallback`].
    PlusFallback,
}

/// A `readdir` reply builder implementing libfuse's size-limited directory
/// buffer protocol.
pub struct DirBuffer {
    req: fuse_req_t,
    capacity: usize,
    buf: Vec<u8>,
    format: DirBufferFormat,
}

impl DirBuffer {
    pub(crate) fn new(req: fuse_req_t, size: usize) -> Self {
        DirBuffer {
            req,
            capacity: size,
            buf: Vec::new(),
            format: DirBufferFormat::Plain,
        }
    }

    /// Like [`DirBuffer::new`], but internally emits entries in the
    /// READDIRPLUS wire format (via `fuse_add_direntry_plus`) instead of
    /// plain READDIR, while keeping the exact same [`DirBuffer::add`]
    /// signature.
    ///
    /// libfuse advertises the READDIRPLUS capability to the kernel
    /// whenever `fuse_lowlevel_ops::readdirplus` is non-NULL (see
    /// `do_init` in libfuse's `fuse_lowlevel.c`) - and this crate always
    /// registers it, since `Filesystem::readdirplus` is always present
    /// (with a default `ENOSYS` body). Once advertised, the kernel sends
    /// READDIRPLUS for every directory listing and never falls back to
    /// plain READDIR on a per-request basis. So a filesystem that only
    /// implements `Filesystem::readdir` would otherwise see every `ls`
    /// fail with ENOSYS. `session.rs`'s `readdirplus` trampoline uses this
    /// constructor to synthesize a READDIRPLUS-shaped reply from
    /// `readdir`'s output as a fallback in that case.
    ///
    /// Each synthesized entry uses nodeid (`fuse_entry_param::ino`) `0`,
    /// which tells the kernel "no lookup was performed for this entry":
    /// the kernel's `fuse_direntplus_link()` skips linking/caching entries
    /// with nodeid 0, so this does not increment any lookup count and does
    /// not require a matching `Filesystem::forget` call.
    pub(crate) fn new_plus_fallback(req: fuse_req_t, size: usize) -> Self {
        DirBuffer {
            req,
            capacity: size,
            buf: Vec::new(),
            format: DirBufferFormat::PlusFallback,
        }
    }

    /// Adds one directory entry. Returns `false` (without adding anything)
    /// once the buffer is full, at which point the caller should stop
    /// iterating; returns `true` otherwise (including when a filename with
    /// an interior NUL byte - which cannot occur for real filenames - was
    /// silently skipped).
    pub fn add(&mut self, name: &str, ino: Inode, kind: FileType, next_offset: u64) -> bool {
        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return true,
        };

        let mut st: stat = unsafe { std::mem::zeroed() };
        st.st_ino = ino as _;
        st.st_mode = kind.to_mode_bits() as _;

        match self.format {
            DirBufferFormat::Plain => {
                let entry_size =
                    raw_add_direntry(self.req, ptr::null_mut(), 0, cname.as_ptr(), ptr::null(), 0);

                if self.buf.len() + entry_size > self.capacity {
                    return false;
                }

                let old_len = self.buf.len();
                self.buf.resize(old_len + entry_size, 0);
                unsafe {
                    raw_add_direntry(
                        self.req,
                        self.buf.as_mut_ptr().add(old_len) as *mut c_char,
                        entry_size,
                        cname.as_ptr(),
                        &st,
                        next_offset as _,
                    );
                }
            }
            DirBufferFormat::PlusFallback => {
                let param = fuse_entry_param {
                    ino: 0,
                    generation: 0,
                    attr: st,
                    attr_timeout: 0.0,
                    entry_timeout: 0.0,
                };

                let entry_size = raw_add_direntry_plus(
                    self.req,
                    ptr::null_mut(),
                    0,
                    cname.as_ptr(),
                    ptr::null(),
                    0,
                );

                if self.buf.len() + entry_size > self.capacity {
                    return false;
                }

                let old_len = self.buf.len();
                self.buf.resize(old_len + entry_size, 0);
                unsafe {
                    raw_add_direntry_plus(
                        self.req,
                        self.buf.as_mut_ptr().add(old_len) as *mut c_char,
                        entry_size,
                        cname.as_ptr(),
                        &param,
                        next_offset as _,
                    );
                }
            }
        }
        true
    }

    /// Consumes the buffer, returning ownership of its bytes. `session.rs`
    /// only ever needs a borrowed view (see `as_slice`); kept alongside it
    /// for API completeness.
    #[allow(dead_code)]
    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

/// A `readdirplus` reply builder, the `Entry`-aware counterpart of
/// [`DirBuffer`].
pub struct DirPlusBuffer {
    req: fuse_req_t,
    capacity: usize,
    buf: Vec<u8>,
}

impl DirPlusBuffer {
    pub(crate) fn new(req: fuse_req_t, size: usize) -> Self {
        DirPlusBuffer {
            req,
            capacity: size,
            buf: Vec::new(),
        }
    }

    /// Adds one directory entry with full attributes. Same full/skip
    /// semantics as [`DirBuffer::add`].
    pub fn add(&mut self, name: &str, entry: &Entry, next_offset: u64) -> bool {
        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return true,
        };

        let param = entry.to_entry_param();

        let entry_size = raw_add_direntry_plus(
            self.req,
            ptr::null_mut(),
            0,
            cname.as_ptr(),
            ptr::null(),
            0,
        );

        if self.buf.len() + entry_size > self.capacity {
            return false;
        }

        let old_len = self.buf.len();
        self.buf.resize(old_len + entry_size, 0);
        unsafe {
            raw_add_direntry_plus(
                self.req,
                self.buf.as_mut_ptr().add(old_len) as *mut c_char,
                entry_size,
                cname.as_ptr(),
                &param,
                next_offset as _,
            );
        }
        true
    }

    /// Consumes the buffer, returning ownership of its bytes. `session.rs`
    /// only ever needs a borrowed view (see `as_slice`); kept alongside it
    /// for API completeness.
    #[allow(dead_code)]
    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

// ---------------------------------------------------------------------
// ConnInfo
// ---------------------------------------------------------------------

/// A thin, mutable wrapper over `fuse_conn_info`, passed to `Filesystem::init`.
pub struct ConnInfo {
    conn: *mut fuse_conn_info,
}

impl ConnInfo {
    pub(crate) fn new(conn: *mut fuse_conn_info) -> Self {
        ConnInfo { conn }
    }

    pub fn proto_major(&self) -> u32 {
        unsafe { (*self.conn).proto_major }
    }

    pub fn proto_minor(&self) -> u32 {
        unsafe { (*self.conn).proto_minor }
    }

    pub fn max_write(&self) -> u32 {
        unsafe { (*self.conn).max_write }
    }

    pub fn max_readahead(&self) -> u32 {
        unsafe { (*self.conn).max_readahead }
    }

    pub fn set_max_write(&mut self, value: u32) {
        unsafe { (*self.conn).max_write = value };
    }

    pub fn set_max_readahead(&mut self, value: u32) {
        unsafe { (*self.conn).max_readahead = value };
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_type_mode_round_trip() {
        for kind in [
            FileType::RegularFile,
            FileType::Directory,
            FileType::Symlink,
            FileType::CharDevice,
            FileType::BlockDevice,
            FileType::NamedPipe,
            FileType::Socket,
        ] {
            let bits = kind.to_mode_bits();
            assert_eq!(FileType::from_mode_bits(bits), Some(kind));
            // Permission bits mixed in should not affect decoding.
            assert_eq!(FileType::from_mode_bits(bits | 0o644), Some(kind));
        }
    }

    #[test]
    fn file_type_from_mode_bits_unknown() {
        assert_eq!(FileType::from_mode_bits(0), None);
    }

    #[test]
    fn file_attr_to_stat_basic_fields() {
        let attr = FileAttr {
            ino: 42,
            size: 1234,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 3,
            ..Default::default()
        };
        let st = attr.to_stat();
        assert_eq!(st.st_ino as u64, 42);
        assert_eq!(st.st_size as u64, 1234);
        assert_eq!(st.st_nlink as u64, 3);
        assert_eq!(
            FileType::from_mode_bits(st.st_mode as u32),
            Some(FileType::RegularFile)
        );
        assert_eq!((st.st_mode as u32) & 0o777, 0o644);
    }

    #[test]
    fn file_attr_to_stat_directory_mode() {
        let attr = FileAttr {
            kind: FileType::Directory,
            perm: 0o755,
            ..Default::default()
        };
        let st = attr.to_stat();
        assert_eq!(
            FileType::from_mode_bits(st.st_mode as u32),
            Some(FileType::Directory)
        );
        assert_eq!((st.st_mode as u32) & 0o777, 0o755);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn file_attr_to_stat_times() {
        let attr = FileAttr {
            atime: UNIX_EPOCH + Duration::new(1000, 500),
            mtime: UNIX_EPOCH + Duration::new(2000, 0),
            ..Default::default()
        };
        let st = attr.to_stat();
        assert_eq!(st.st_atimespec.tv_sec, 1000);
        assert_eq!(st.st_atimespec.tv_nsec, 500);
        assert_eq!(st.st_mtimespec.tv_sec, 2000);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn file_attr_to_stat_pre_epoch_time() {
        // 1.5 seconds before the epoch.
        let t = UNIX_EPOCH - Duration::new(1, 500_000_000);
        let attr = FileAttr {
            atime: t,
            ..Default::default()
        };
        let st = attr.to_stat();
        assert_eq!(st.st_atimespec.tv_sec, -2);
        assert_eq!(st.st_atimespec.tv_nsec, 500_000_000);
    }

    #[test]
    fn secs_nsecs_round_trip_pre_and_post_epoch() {
        for t in [
            UNIX_EPOCH,
            UNIX_EPOCH + Duration::new(100, 250),
            UNIX_EPOCH - Duration::new(1, 0),
            UNIX_EPOCH - Duration::new(1, 500_000_000),
            UNIX_EPOCH - Duration::new(100, 999_999_999),
        ] {
            let (secs, nsecs) = system_time_to_secs_nsecs(t);
            let back = secs_nsecs_to_system_time(secs, nsecs);
            assert_eq!(back, t, "round trip failed for {:?}", t);
        }
    }

    #[test]
    fn entry_to_entry_param_timeouts() {
        let entry = Entry {
            ino: 7,
            generation: 1,
            attr_timeout: Duration::from_millis(1500),
            entry_timeout: Duration::from_secs(2),
            ..Default::default()
        };
        let param = entry.to_entry_param();
        assert_eq!(param.ino, 7);
        assert_eq!(param.generation, 1);
        assert!((param.attr_timeout - 1.5).abs() < 1e-9);
        assert!((param.entry_timeout - 2.0).abs() < 1e-9);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn set_attrs_from_raw_decodes_bitmask() {
        let mut st: stat = unsafe { std::mem::zeroed() };
        st.st_mode = 0o644;
        st.st_size = 4096;
        st.st_mtimespec = timespec {
            tv_sec: 12345,
            tv_nsec: 6789,
        };

        let to_set = (FUSE_SET_ATTR_MODE
            | FUSE_SET_ATTR_SIZE
            | FUSE_SET_ATTR_ATIME_NOW
            | FUSE_SET_ATTR_MTIME) as c_int;

        let attrs = SetAttrs::from_raw(&st, to_set);
        assert_eq!(attrs.mode, Some(0o644));
        assert_eq!(attrs.size, Some(4096));
        assert_eq!(attrs.atime, Some(TimeOrNow::Now));
        assert_eq!(
            attrs.mtime,
            Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                12345, 6789
            )))
        );
        assert_eq!(attrs.uid, None);
        assert_eq!(attrs.gid, None);
        assert_eq!(attrs.ctime, None);
    }

    #[test]
    fn errno_from_io_error() {
        let io_err = std::io::Error::from_raw_os_error(libc::ENOENT);
        let errno: Errno = io_err.into();
        assert_eq!(errno, Errno::ENOENT);

        let other_err = std::io::Error::other("boom");
        let errno: Errno = other_err.into();
        assert_eq!(errno, Errno::EIO);
    }

    #[test]
    fn errno_raw_and_from_raw() {
        let e = Errno::from_raw(5);
        assert_eq!(e.raw(), 5);
        assert_eq!(Errno::from(5), e);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn statfs_conversion_spot_check() {
        let sfs = StatFs {
            blocks: 1000,
            bfree: 500,
            bavail: 400,
            files: 100,
            ffree: 50,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        };
        let raw = sfs.to_raw();
        assert_eq!(raw.f_blocks, 1000);
        assert_eq!(raw.f_bfree, 500);
        assert_eq!(raw.f_bavail, 400);
        assert_eq!(raw.f_files, 100);
        assert_eq!(raw.f_ffree, 50);
        assert_eq!(raw.f_bsize, 4096);
    }
}
