//! Trampolines from the raw `fuse_lowlevel_ops` C callback table to a safe
//! [`Filesystem`] implementation, plus the [`Session`] type that owns a
//! mounted FUSE session end to end.
//!
//! # Threading
//!
//! `fuse_session_loop` (used by [`Session::run`]) processes requests
//! strictly one at a time, so at most one trampoline below is ever
//! executing for a given [`Filesystem`] instance. That is what makes it
//! sound to recover a `&mut FsHolder<F>` from the raw `userdata`/`req`
//! pointer in every shim.
//!
//! # Panics
//!
//! Every shim that can reply with an error wraps its call into the
//! [`Filesystem`] trait in [`catch_unwind`]; a panicking filesystem method
//! results in `EIO` being sent back to the kernel instead of unwinding
//! across the `extern "C"` boundary (which is undefined behavior). `forget`
//! and `forget_multi` have no error reply available in the FUSE protocol
//! (only `fuse_reply_none`), so a panic there is caught, logged to stderr,
//! and swallowed.
//!
//! # Darwin symbol aliasing
//!
//! See `darwin.rs` for the macOS-only workaround this module relies on:
//! `fuse_reply_entry`, `fuse_reply_attr`, `fuse_reply_create`,
//! `fuse_reply_statfs`, `fuse_add_direntry` and `fuse_add_direntry_plus` are
//! aliased to `<name>$DARWIN` symbols in the headers that do not exist in
//! the installed dylib; this module always goes through the small
//! `raw_reply_*` wrappers below (cfg'd per platform) instead of calling the
//! bindgen declarations directly, so the rest of this file stays
//! `#[cfg]`-free.
//!
//! # Darwin-extended callback argument types
//!
//! `Filesystem::setattr`'s underlying `fuse_lowlevel_ops::setattr` field is
//! typed `*mut fuse_darwin_attr` by bindgen on macOS (the header was parsed
//! with Darwin extensions enabled by default). Since this crate always
//! disables Darwin extensions at the session level
//! (`set_darwin_extensions_enabled(0)`, see [`Session::new`]), libfuse
//! actually invokes the callback with a vanilla `struct stat*` at runtime;
//! `setattr_shim` reinterprets the pointer accordingly (see the safety
//! comment on that function). No other op's callback signature is affected.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::panic::{self, AssertUnwindSafe};
use std::ptr;
use std::time::Duration;

use libfuse_sys::fuse_lowlevel::{
    dev_t, fuse_args, fuse_conn_info, fuse_entry_param, fuse_file_info, fuse_forget_data,
    fuse_ino_t, fuse_lowlevel_ops, fuse_opt_free_args, fuse_remove_signal_handlers,
    fuse_reply_buf, fuse_reply_err, fuse_reply_lseek, fuse_reply_none, fuse_reply_open,
    fuse_reply_readlink, fuse_reply_write, fuse_reply_xattr, fuse_req_t, fuse_req_userdata,
    fuse_session, fuse_session_destroy, fuse_session_loop, fuse_session_mount,
    fuse_session_new_versioned, fuse_session_unmount, fuse_set_signal_handlers, libfuse_version,
    mode_t, off_t, stat, FUSE_HOTFIX_VERSION, FUSE_MAJOR_VERSION, FUSE_MINOR_VERSION,
};

#[cfg(target_os = "macos")]
use libfuse_sys::fuse_lowlevel::{fuse_darwin_attr, statfs};
#[cfg(not(target_os = "macos"))]
use libfuse_sys::fuse_lowlevel::{
    fuse_reply_attr, fuse_reply_create, fuse_reply_entry, fuse_reply_statfs, statvfs,
};

#[cfg(target_os = "macos")]
use crate::darwin::{
    fuse_reply_attr_vanilla, fuse_reply_create_vanilla, fuse_reply_entry_vanilla,
    fuse_reply_statfs_vanilla,
};

use crate::filesystem::Filesystem;
use crate::types::{ConnInfo, DirBuffer, DirPlusBuffer, Entry, Errno, FileAttr, FileInfo, Request, SetAttrs};

// ---------------------------------------------------------------------
// Darwin-aliased reply wrappers (see module docs)
// ---------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn raw_reply_entry(req: fuse_req_t, e: *const fuse_entry_param) -> c_int {
    unsafe { fuse_reply_entry_vanilla(req, e) }
}
#[cfg(not(target_os = "macos"))]
fn raw_reply_entry(req: fuse_req_t, e: *const fuse_entry_param) -> c_int {
    unsafe { fuse_reply_entry(req, e) }
}

#[cfg(target_os = "macos")]
fn raw_reply_attr(req: fuse_req_t, attr: *const stat, timeout: f64) -> c_int {
    unsafe { fuse_reply_attr_vanilla(req, attr, timeout) }
}
#[cfg(not(target_os = "macos"))]
fn raw_reply_attr(req: fuse_req_t, attr: *const stat, timeout: f64) -> c_int {
    unsafe { fuse_reply_attr(req, attr, timeout) }
}

#[cfg(target_os = "macos")]
fn raw_reply_create(req: fuse_req_t, e: *const fuse_entry_param, fi: *const fuse_file_info) -> c_int {
    unsafe { fuse_reply_create_vanilla(req, e, fi) }
}
#[cfg(not(target_os = "macos"))]
fn raw_reply_create(req: fuse_req_t, e: *const fuse_entry_param, fi: *const fuse_file_info) -> c_int {
    unsafe { fuse_reply_create(req, e, fi) }
}

#[cfg(target_os = "macos")]
fn raw_reply_statfs(req: fuse_req_t, stbuf: *const statfs) -> c_int {
    unsafe { fuse_reply_statfs_vanilla(req, stbuf) }
}
#[cfg(not(target_os = "macos"))]
fn raw_reply_statfs(req: fuse_req_t, stbuf: *const statvfs) -> c_int {
    unsafe { fuse_reply_statfs(req, stbuf) }
}

// ---------------------------------------------------------------------
// FsHolder
// ---------------------------------------------------------------------

/// Owns a filesystem implementation for the lifetime of a [`Session`].
/// Boxed and handed to libfuse as opaque `userdata`; recovered by the
/// trampolines below via `fuse_req_userdata` (or, for `init`/`destroy`,
/// the userdata pointer libfuse passes directly).
struct FsHolder<F: Filesystem> {
    fs: F,
}

/// Recovers the `FsHolder<F>` associated with an in-flight request.
///
/// Sound because `fuse_session_loop` (the only driver of requests this
/// crate supports) processes requests sequentially: no two trampolines for
/// the same session are ever running concurrently, so this unique
/// `&mut` never aliases another live reference.
fn holder_of<'a, F: Filesystem>(req: fuse_req_t) -> &'a mut FsHolder<F> {
    unsafe { &mut *(fuse_req_userdata(req) as *mut FsHolder<F>) }
}

// ---------------------------------------------------------------------
// small shared helpers
// ---------------------------------------------------------------------

/// Decodes a non-null, NUL-terminated C string as UTF-8.
fn c_str<'a>(ptr: *const c_char) -> Result<&'a str, Errno> {
    unsafe { CStr::from_ptr(ptr) }.to_str().map_err(|_| Errno::EILSEQ)
}

/// Decodes a name argument, replying `EILSEQ` (without calling into the
/// filesystem) and returning from the enclosing `unsafe extern "C" fn` on
/// invalid UTF-8.
macro_rules! try_name {
    ($req:expr, $ptr:expr) => {
        match c_str($ptr) {
            Ok(s) => s,
            Err(e) => {
                reply_err($req, e);
                return;
            }
        }
    };
}

/// Runs `f`, converting a panic into `Err(Errno::EIO)` instead of
/// unwinding across the `extern "C"` boundary.
fn catch_unwind<T>(f: impl FnOnce() -> Result<T, Errno>) -> Result<T, Errno> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(Errno::EIO),
    }
}

/// Like [`catch_unwind`], but for the `Filesystem` methods that have no
/// error reply (`init`, `destroy`, `forget`): a panic is logged and
/// swallowed rather than propagated.
fn catch_unwind_unit(f: impl FnOnce()) {
    if panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
        eprintln!("fuse3: filesystem callback panicked; no reply is possible for this operation");
    }
}

fn reply_err(req: fuse_req_t, errno: Errno) {
    unsafe { fuse_reply_err(req, errno.raw()) };
}

fn reply_ok(req: fuse_req_t) {
    unsafe { fuse_reply_err(req, 0) };
}

fn reply_entry(req: fuse_req_t, entry: &Entry) {
    let param = entry.to_entry_param();
    raw_reply_entry(req, &param);
}

fn reply_attr(req: fuse_req_t, attr: &FileAttr, timeout: Duration) {
    let st = attr.to_stat();
    raw_reply_attr(req, &st, timeout.as_secs_f64());
}

/// Sends a `readdir`/`readdirplus` buffer, translating an empty buffer to
/// the null/zero-size reply that signals end-of-stream.
fn reply_dir_buf(req: fuse_req_t, data: &[u8]) {
    if data.is_empty() {
        unsafe { fuse_reply_buf(req, ptr::null(), 0) };
    } else {
        unsafe { fuse_reply_buf(req, data.as_ptr() as *const c_char, data.len()) };
    }
}

/// Implements the `getxattr`/`listxattr` size-query protocol: a requested
/// size of zero asks for just the value's length; otherwise the value is
/// sent if it fits, or `ERANGE` if it doesn't.
fn reply_xattr_data(req: fuse_req_t, data: &[u8], requested_size: usize) {
    if requested_size == 0 {
        unsafe { fuse_reply_xattr(req, data.len()) };
    } else if data.len() > requested_size {
        reply_err(req, Errno::ERANGE);
    } else {
        unsafe { fuse_reply_buf(req, data.as_ptr() as *const c_char, data.len()) };
    }
}

// ---------------------------------------------------------------------
// Trampolines
// ---------------------------------------------------------------------

unsafe extern "C" fn init_shim<F: Filesystem>(userdata: *mut c_void, conn: *mut fuse_conn_info) {
    let holder = unsafe { &mut *(userdata as *mut FsHolder<F>) };
    let mut conn_info = ConnInfo::new(conn);
    catch_unwind_unit(|| holder.fs.init(&mut conn_info));
}

unsafe extern "C" fn destroy_shim<F: Filesystem>(userdata: *mut c_void) {
    let holder = unsafe { &mut *(userdata as *mut FsHolder<F>) };
    catch_unwind_unit(|| holder.fs.destroy());
}

unsafe extern "C" fn lookup_shim<F: Filesystem>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
) {
    let name = try_name!(req, name);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.lookup(&request, parent, name)) {
        Ok(entry) => reply_entry(req, &entry),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn forget_shim<F: Filesystem>(req: fuse_req_t, ino: fuse_ino_t, nlookup: u64) {
    let holder = holder_of::<F>(req);
    catch_unwind_unit(|| holder.fs.forget(ino, nlookup));
    unsafe { fuse_reply_none(req) };
}

unsafe extern "C" fn forget_multi_shim<F: Filesystem>(
    req: fuse_req_t,
    count: usize,
    forgets: *mut fuse_forget_data,
) {
    let holder = holder_of::<F>(req);
    let entries = unsafe { std::slice::from_raw_parts(forgets, count) };
    for entry in entries {
        catch_unwind_unit(|| holder.fs.forget(entry.ino, entry.nlookup));
    }
    unsafe { fuse_reply_none(req) };
}

unsafe extern "C" fn getattr_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let fh = FileInfo::from_raw(fi).map(|f| f.fh);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.getattr(&request, ino, fh)) {
        Ok((attr, timeout)) => reply_attr(req, &attr, timeout),
        Err(e) => reply_err(req, e),
    }
}

/// The shared implementation behind the per-platform `setattr_shim`
/// wrappers below (which only differ in the raw `attr` pointer's declared
/// type).
fn setattr_shim_impl<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    attrs: &SetAttrs,
    fi: *mut fuse_file_info,
) {
    let fh = FileInfo::from_raw(fi).map(|f| f.fh);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.setattr(&request, ino, attrs, fh)) {
        Ok((attr, timeout)) => reply_attr(req, &attr, timeout),
        Err(e) => reply_err(req, e),
    }
}

// SAFETY (macOS): bindgen types this callback's `attr` argument as
// `*mut fuse_darwin_attr` because the header was parsed with Darwin
// extensions enabled by default. This crate always disables Darwin
// extensions at the session level (`set_darwin_extensions_enabled(0)`, see
// `Session::new`), so libfuse actually passes a vanilla `struct stat*` at
// runtime; reinterpreting the pointer as `*const stat` below is what makes
// `SetAttrs::from_raw` (which expects the vanilla layout) correct.
#[cfg(target_os = "macos")]
unsafe extern "C" fn setattr_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    attr: *mut fuse_darwin_attr,
    to_set: c_int,
    fi: *mut fuse_file_info,
) {
    let attrs = SetAttrs::from_raw(attr as *const stat, to_set);
    setattr_shim_impl::<F>(req, ino, &attrs, fi);
}
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn setattr_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    attr: *mut stat,
    to_set: c_int,
    fi: *mut fuse_file_info,
) {
    let attrs = SetAttrs::from_raw(attr as *const stat, to_set);
    setattr_shim_impl::<F>(req, ino, &attrs, fi);
}

unsafe extern "C" fn readlink_shim<F: Filesystem>(req: fuse_req_t, ino: fuse_ino_t) {
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.readlink(&request, ino)) {
        Ok(target) => match CString::new(target) {
            Ok(c) => {
                unsafe { fuse_reply_readlink(req, c.as_ptr()) };
            }
            Err(_) => reply_err(req, Errno::EINVAL),
        },
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn mknod_shim<F: Filesystem>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
    mode: mode_t,
    rdev: dev_t,
) {
    let name = try_name!(req, name);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.mknod(&request, parent, name, mode as u32, rdev as u32)) {
        Ok(entry) => reply_entry(req, &entry),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn mkdir_shim<F: Filesystem>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
    mode: mode_t,
) {
    let name = try_name!(req, name);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.mkdir(&request, parent, name, mode as u32)) {
        Ok(entry) => reply_entry(req, &entry),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn unlink_shim<F: Filesystem>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
) {
    let name = try_name!(req, name);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.unlink(&request, parent, name)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn rmdir_shim<F: Filesystem>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
) {
    let name = try_name!(req, name);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.rmdir(&request, parent, name)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn symlink_shim<F: Filesystem>(
    req: fuse_req_t,
    link: *const c_char,
    parent: fuse_ino_t,
    name: *const c_char,
) {
    let link = try_name!(req, link);
    let name = try_name!(req, name);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.symlink(&request, parent, name, link)) {
        Ok(entry) => reply_entry(req, &entry),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn rename_shim<F: Filesystem>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
    newparent: fuse_ino_t,
    newname: *const c_char,
    flags: c_uint,
) {
    let name = try_name!(req, name);
    let newname = try_name!(req, newname);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| {
        holder
            .fs
            .rename(&request, parent, name, newparent, newname, flags)
    }) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn link_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    newparent: fuse_ino_t,
    newname: *const c_char,
) {
    let newname = try_name!(req, newname);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.link(&request, ino, newparent, newname)) {
        Ok(entry) => reply_entry(req, &entry),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn open_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.open(&request, ino, &file_info)) {
        Ok(reply) => {
            reply.apply(fi);
            unsafe { fuse_reply_open(req, fi as *const fuse_file_info) };
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn read_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    size: usize,
    off: off_t,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.read(&request, ino, size, off as u64, &file_info)) {
        Ok(data) => {
            let len = data.len().min(size);
            unsafe { fuse_reply_buf(req, data[..len].as_ptr() as *const c_char, len) };
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn write_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    buf: *const c_char,
    size: usize,
    off: off_t,
    fi: *mut fuse_file_info,
) {
    let data = unsafe { std::slice::from_raw_parts(buf as *const u8, size) };
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.write(&request, ino, data, off as u64, &file_info)) {
        Ok(count) => {
            unsafe { fuse_reply_write(req, count) };
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn flush_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.flush(&request, ino, &file_info)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn release_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.release(&request, ino, &file_info)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn fsync_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    datasync: c_int,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.fsync(&request, ino, datasync != 0, &file_info)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn opendir_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.opendir(&request, ino, &file_info)) {
        Ok(reply) => {
            reply.apply(fi);
            unsafe { fuse_reply_open(req, fi as *const fuse_file_info) };
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn readdir_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    size: usize,
    off: off_t,
    fi: *mut fuse_file_info,
) {
    let fh = FileInfo::from_raw(fi).map(|f| f.fh).unwrap_or(0);
    let mut buf = DirBuffer::new(req, size);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.readdir(&request, ino, off as u64, fh, &mut buf)) {
        Ok(()) => reply_dir_buf(req, buf.as_slice()),
        Err(e) => reply_err(req, e),
    }
}

// libfuse advertises the READDIRPLUS capability to the kernel whenever
// `fuse_lowlevel_ops::readdirplus` is non-NULL (see `do_init` in libfuse's
// `fuse_lowlevel.c`), and this crate always registers it (`Filesystem`
// always has a `readdirplus`, defaulting to `ENOSYS`). Once advertised,
// the kernel sends READDIRPLUS for every directory listing and never
// falls back to plain READDIR on a per-request basis - so a filesystem
// that only implements `Filesystem::readdir` (the common case) would
// otherwise see every `ls`/directory listing fail with ENOSYS. To avoid
// that, an `ENOSYS` from `readdirplus` falls back to calling `readdir`
// and synthesizing a READDIRPLUS-shaped reply from it via
// `DirBuffer::new_plus_fallback` (see its doc comment for details).
unsafe extern "C" fn readdirplus_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    size: usize,
    off: off_t,
    fi: *mut fuse_file_info,
) {
    let fh = FileInfo::from_raw(fi).map(|f| f.fh).unwrap_or(0);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);

    let mut buf = DirPlusBuffer::new(req, size);
    match catch_unwind(|| holder.fs.readdirplus(&request, ino, off as u64, fh, &mut buf)) {
        Ok(()) => reply_dir_buf(req, buf.as_slice()),
        Err(Errno::ENOSYS) => {
            let mut fallback = DirBuffer::new_plus_fallback(req, size);
            match catch_unwind(|| holder.fs.readdir(&request, ino, off as u64, fh, &mut fallback)) {
                Ok(()) => reply_dir_buf(req, fallback.as_slice()),
                Err(e) => reply_err(req, e),
            }
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn releasedir_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.releasedir(&request, ino, &file_info)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn fsyncdir_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    datasync: c_int,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.fsyncdir(&request, ino, datasync != 0, &file_info)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn statfs_shim<F: Filesystem>(req: fuse_req_t, ino: fuse_ino_t) {
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.statfs(&request, ino)) {
        Ok(stats) => {
            let raw = stats.to_raw();
            raw_reply_statfs(req, &raw);
        }
        Err(e) => reply_err(req, e),
    }
}

fn setxattr_shim_impl<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    value: *const c_char,
    size: usize,
    flags: c_int,
) {
    let name = try_name!(req, name);
    let value = unsafe { std::slice::from_raw_parts(value as *const u8, size) };
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.setxattr(&request, ino, name, value, flags)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

// The macOS callback carries an extra `position` argument (used by
// resource-fork-aware clients); this crate does not expose it in the
// portable `Filesystem::setxattr` signature, so it is simply ignored.
#[cfg(target_os = "macos")]
unsafe extern "C" fn setxattr_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    value: *const c_char,
    size: usize,
    flags: c_int,
    _position: u32,
) {
    setxattr_shim_impl::<F>(req, ino, name, value, size, flags);
}
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn setxattr_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    value: *const c_char,
    size: usize,
    flags: c_int,
) {
    setxattr_shim_impl::<F>(req, ino, name, value, size, flags);
}

fn getxattr_shim_impl<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    size: usize,
) {
    let name = try_name!(req, name);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.getxattr(&request, ino, name)) {
        Ok(data) => reply_xattr_data(req, &data, size),
        Err(e) => reply_err(req, e),
    }
}

// Same macOS-only `position` argument as `setxattr_shim`; ignored.
#[cfg(target_os = "macos")]
unsafe extern "C" fn getxattr_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    size: usize,
    _position: u32,
) {
    getxattr_shim_impl::<F>(req, ino, name, size);
}
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn getxattr_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    size: usize,
) {
    getxattr_shim_impl::<F>(req, ino, name, size);
}

unsafe extern "C" fn listxattr_shim<F: Filesystem>(req: fuse_req_t, ino: fuse_ino_t, size: usize) {
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.listxattr(&request, ino)) {
        Ok(data) => reply_xattr_data(req, &data, size),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn removexattr_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
) {
    let name = try_name!(req, name);
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.removexattr(&request, ino, name)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn access_shim<F: Filesystem>(req: fuse_req_t, ino: fuse_ino_t, mask: c_int) {
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.access(&request, ino, mask)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn create_shim<F: Filesystem>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
    mode: mode_t,
    fi: *mut fuse_file_info,
) {
    let name = try_name!(req, name);
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.create(&request, parent, name, mode as u32, &file_info)) {
        Ok((entry, reply)) => {
            reply.apply(fi);
            let param = entry.to_entry_param();
            raw_reply_create(req, &param, fi as *const fuse_file_info);
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn fallocate_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    mode: c_int,
    offset: off_t,
    length: off_t,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| {
        holder
            .fs
            .fallocate(&request, ino, mode, offset as u64, length as u64, &file_info)
    }) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn lseek_shim<F: Filesystem>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    off: off_t,
    whence: c_int,
    fi: *mut fuse_file_info,
) {
    let file_info = FileInfo::from_raw(fi).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| holder.fs.lseek(&request, ino, off as u64, whence, &file_info)) {
        Ok(new_off) => {
            unsafe { fuse_reply_lseek(req, new_off as off_t) };
        }
        Err(e) => reply_err(req, e),
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn copy_file_range_shim<F: Filesystem>(
    req: fuse_req_t,
    ino_in: fuse_ino_t,
    off_in: off_t,
    fi_in: *mut fuse_file_info,
    ino_out: fuse_ino_t,
    off_out: off_t,
    fi_out: *mut fuse_file_info,
    len: usize,
    flags: c_int,
) {
    let file_info_in = FileInfo::from_raw(fi_in).unwrap_or_default();
    let file_info_out = FileInfo::from_raw(fi_out).unwrap_or_default();
    let holder = holder_of::<F>(req);
    let request = Request::new(req);
    match catch_unwind(|| {
        holder.fs.copy_file_range(
            &request,
            ino_in,
            off_in as u64,
            &file_info_in,
            ino_out,
            off_out as u64,
            &file_info_out,
            len as u64,
            flags,
        )
    }) {
        Ok(count) => {
            unsafe { fuse_reply_write(req, count) };
        }
        Err(e) => reply_err(req, e),
    }
}

// ---------------------------------------------------------------------
// Ops table
// ---------------------------------------------------------------------

/// Builds a `fuse_lowlevel_ops` table with every operation covered by
/// [`Filesystem`] wired up to its trampoline. Operations this crate does
/// not cover (`getlk`/`setlk`/`flock`, `ioctl`, `poll`, `bmap`,
/// `write_buf`, `retrieve_reply`, `statx`, `tmpfile`,
/// `setvolname`/`monitor` and other macOS-only extensions, ...) are left
/// `None`.
pub(crate) fn make_ops<F: Filesystem>() -> fuse_lowlevel_ops {
    fuse_lowlevel_ops {
        init: Some(init_shim::<F>),
        destroy: Some(destroy_shim::<F>),
        lookup: Some(lookup_shim::<F>),
        forget: Some(forget_shim::<F>),
        getattr: Some(getattr_shim::<F>),
        setattr: Some(setattr_shim::<F>),
        readlink: Some(readlink_shim::<F>),
        mknod: Some(mknod_shim::<F>),
        mkdir: Some(mkdir_shim::<F>),
        unlink: Some(unlink_shim::<F>),
        rmdir: Some(rmdir_shim::<F>),
        symlink: Some(symlink_shim::<F>),
        rename: Some(rename_shim::<F>),
        link: Some(link_shim::<F>),
        open: Some(open_shim::<F>),
        read: Some(read_shim::<F>),
        write: Some(write_shim::<F>),
        flush: Some(flush_shim::<F>),
        release: Some(release_shim::<F>),
        fsync: Some(fsync_shim::<F>),
        opendir: Some(opendir_shim::<F>),
        readdir: Some(readdir_shim::<F>),
        releasedir: Some(releasedir_shim::<F>),
        fsyncdir: Some(fsyncdir_shim::<F>),
        statfs: Some(statfs_shim::<F>),
        setxattr: Some(setxattr_shim::<F>),
        getxattr: Some(getxattr_shim::<F>),
        listxattr: Some(listxattr_shim::<F>),
        removexattr: Some(removexattr_shim::<F>),
        access: Some(access_shim::<F>),
        create: Some(create_shim::<F>),
        forget_multi: Some(forget_multi_shim::<F>),
        fallocate: Some(fallocate_shim::<F>),
        readdirplus: Some(readdirplus_shim::<F>),
        copy_file_range: Some(copy_file_range_shim::<F>),
        lseek: Some(lseek_shim::<F>),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------
// MountOption
// ---------------------------------------------------------------------

/// A libfuse mount option, rendered to a `-o key[=value]` argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountOption {
    /// Mount read-only (`-o ro`).
    ReadOnly,
    /// Allow other users to access the mount (`-o allow_other`).
    AllowOther,
    /// Automatically unmount when the owning process exits
    /// (`-o auto_unmount`).
    AutoUnmount,
    /// Let the kernel perform its own permission checks
    /// (`-o default_permissions`).
    DefaultPermissions,
    /// Sets the filesystem name shown by `mount`/`df` (`-o fsname=NAME`).
    FsName(String),
    /// Sets the filesystem subtype (`-o subtype=NAME`).
    Subtype(String),
    /// Any other raw `-o` option, passed through verbatim.
    Custom(String),
}

impl MountOption {
    fn render(&self) -> String {
        match self {
            MountOption::ReadOnly => "ro".to_string(),
            MountOption::AllowOther => "allow_other".to_string(),
            MountOption::AutoUnmount => "auto_unmount".to_string(),
            MountOption::DefaultPermissions => "default_permissions".to_string(),
            MountOption::FsName(name) => format!("fsname={name}"),
            MountOption::Subtype(name) => format!("subtype={name}"),
            MountOption::Custom(opt) => opt.clone(),
        }
    }
}

// ---------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------

/// Errors returned by [`Session`] setup/teardown.
#[derive(Debug)]
pub enum Error {
    /// `fuse_session_new_versioned` returned null.
    SessionNew,
    /// `fuse_session_mount` failed.
    Mount,
    /// `fuse_set_signal_handlers` failed.
    SignalHandlers,
    /// `fuse_session_loop` returned a non-zero code.
    Loop(i32),
    /// The given mountpoint could not be used (e.g. it contains a NUL
    /// byte).
    InvalidMountpoint(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::SessionNew => write!(f, "failed to create the FUSE session"),
            Error::Mount => write!(f, "failed to mount the FUSE session"),
            Error::SignalHandlers => write!(f, "failed to install FUSE signal handlers"),
            Error::Loop(rc) => write!(f, "FUSE session loop exited with code {rc}"),
            Error::InvalidMountpoint(mp) => {
                write!(f, "invalid mountpoint (contains a NUL byte): {mp:?}")
            }
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------

/// A mounted (or mountable) FUSE session driving a single [`Filesystem`]
/// implementation, single-threaded, via `fuse_session_loop`.
///
/// `Session` is `!Send` (it owns raw libfuse pointers that must stay on the
/// thread that created them) - this is enforced implicitly by the raw
/// pointer fields.
pub struct Session<F: Filesystem> {
    session: *mut fuse_session,
    userdata: *mut FsHolder<F>,
    mounted: bool,
    // Kept alive for the lifetime of the session as a defensive measure:
    // nothing in this crate relies on `fuse_session_new_versioned` copying
    // rather than retaining pointers into the `fuse_args` it was given.
    _arg_storage: Vec<CString>,
}

impl<F: Filesystem> Session<F> {
    /// Creates a new (not-yet-mounted) session for `fs`, configured with
    /// `options`.
    pub fn new(fs: F, options: &[MountOption]) -> Result<Self, Error> {
        let mut arg_strings = vec!["fuse3".to_string()];
        if !options.is_empty() {
            let rendered = options
                .iter()
                .map(MountOption::render)
                .collect::<Vec<_>>()
                .join(",");
            arg_strings.push("-o".to_string());
            arg_strings.push(rendered);
        }
        let arg_cstrings: Vec<CString> = arg_strings
            .into_iter()
            .map(|s| CString::new(s).expect("mount option string contains a NUL byte"))
            .collect();
        let mut argv: Vec<*mut c_char> = arg_cstrings.iter().map(|s| s.as_ptr() as *mut c_char).collect();
        let mut args = fuse_args {
            argc: argv.len() as c_int,
            argv: argv.as_mut_ptr(),
            allocated: 0,
        };

        let ops = make_ops::<F>();
        let mut version = libfuse_version {
            major: FUSE_MAJOR_VERSION as _,
            minor: FUSE_MINOR_VERSION as _,
            hotfix: FUSE_HOTFIX_VERSION as _,
            ..Default::default()
        };
        // The reply/trampoline code above always works with the portable
        // vanilla structs, never the Darwin-extended ones - see the module
        // docs.
        #[cfg(target_os = "macos")]
        version.set_darwin_extensions_enabled(0);

        let userdata: *mut FsHolder<F> = Box::into_raw(Box::new(FsHolder { fs }));

        let session = unsafe {
            fuse_session_new_versioned(
                &mut args,
                &ops,
                std::mem::size_of::<fuse_lowlevel_ops>(),
                &mut version,
                userdata as *mut c_void,
            )
        };

        unsafe { fuse_opt_free_args(&mut args) };

        if session.is_null() {
            // Reclaim ownership so the box is freed rather than leaked.
            unsafe { drop(Box::from_raw(userdata)) };
            return Err(Error::SessionNew);
        }

        Ok(Session {
            session,
            userdata,
            mounted: false,
            _arg_storage: arg_cstrings,
        })
    }

    /// Mounts the session at `mountpoint`.
    pub fn mount(&mut self, mountpoint: &str) -> Result<(), Error> {
        let c_mountpoint = CString::new(mountpoint)
            .map_err(|_| Error::InvalidMountpoint(mountpoint.to_string()))?;
        let rc = unsafe { fuse_session_mount(self.session, c_mountpoint.as_ptr()) };
        if rc != 0 {
            return Err(Error::Mount);
        }
        self.mounted = true;
        Ok(())
    }

    /// Runs the single-threaded event loop until the filesystem is
    /// unmounted or a signal terminates it.
    pub fn run(&mut self) -> Result<(), Error> {
        if unsafe { fuse_set_signal_handlers(self.session) } != 0 {
            return Err(Error::SignalHandlers);
        }
        let rc = unsafe { fuse_session_loop(self.session) };
        unsafe { fuse_remove_signal_handlers(self.session) };
        if rc != 0 {
            return Err(Error::Loop(rc));
        }
        Ok(())
    }

    /// Convenience: creates a session for `fs`, mounts it at `mountpoint`,
    /// and runs it to completion. Equivalent to [`Session::new`] +
    /// [`Session::mount`] + [`Session::run`] (unmounting is handled by
    /// `Drop`).
    pub fn mount_and_run(fs: F, mountpoint: &str, options: &[MountOption]) -> Result<(), Error> {
        let mut session = Session::new(fs, options)?;
        session.mount(mountpoint)?;
        session.run()
    }
}

impl<F: Filesystem> Drop for Session<F> {
    fn drop(&mut self) {
        unsafe {
            if self.mounted {
                fuse_session_unmount(self.session);
            }
            fuse_session_destroy(self.session);
            // `destroy_shim` (called synchronously by `fuse_session_destroy`
            // above) only borrows the holder; reclaim ownership now.
            drop(Box::from_raw(self.userdata));
        }
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct NullFs;
    impl Filesystem for NullFs {}

    #[test]
    fn make_ops_registers_covered_callbacks_and_skips_the_rest() {
        let ops = make_ops::<NullFs>();

        // Every op covered by `Filesystem` should be wired up. Linking
        // this test binary also exercises every `raw_reply_*`/vanilla
        // Darwin extern referenced transitively by these shims.
        assert!(ops.init.is_some());
        assert!(ops.destroy.is_some());
        assert!(ops.lookup.is_some());
        assert!(ops.forget.is_some());
        assert!(ops.getattr.is_some());
        assert!(ops.setattr.is_some());
        assert!(ops.readlink.is_some());
        assert!(ops.mknod.is_some());
        assert!(ops.mkdir.is_some());
        assert!(ops.unlink.is_some());
        assert!(ops.rmdir.is_some());
        assert!(ops.symlink.is_some());
        assert!(ops.rename.is_some());
        assert!(ops.link.is_some());
        assert!(ops.open.is_some());
        assert!(ops.read.is_some());
        assert!(ops.write.is_some());
        assert!(ops.flush.is_some());
        assert!(ops.release.is_some());
        assert!(ops.fsync.is_some());
        assert!(ops.opendir.is_some());
        assert!(ops.readdir.is_some());
        assert!(ops.releasedir.is_some());
        assert!(ops.fsyncdir.is_some());
        assert!(ops.statfs.is_some());
        assert!(ops.setxattr.is_some());
        assert!(ops.getxattr.is_some());
        assert!(ops.listxattr.is_some());
        assert!(ops.removexattr.is_some());
        assert!(ops.access.is_some());
        assert!(ops.create.is_some());
        assert!(ops.forget_multi.is_some());
        assert!(ops.fallocate.is_some());
        assert!(ops.readdirplus.is_some());
        assert!(ops.copy_file_range.is_some());
        assert!(ops.lseek.is_some());

        // Explicitly out of scope for this crate.
        assert!(ops.getlk.is_none());
        assert!(ops.setlk.is_none());
        assert!(ops.bmap.is_none());
        assert!(ops.ioctl.is_none());
        assert!(ops.poll.is_none());
        assert!(ops.write_buf.is_none());
        assert!(ops.retrieve_reply.is_none());
        assert!(ops.flock.is_none());
        assert!(ops.tmpfile.is_none());
        assert!(ops.setvolname.is_none());
        assert!(ops.monitor.is_none());
        assert!(ops.statx.is_none());
    }

    #[test]
    fn mount_option_render() {
        assert_eq!(MountOption::ReadOnly.render(), "ro");
        assert_eq!(MountOption::AllowOther.render(), "allow_other");
        assert_eq!(MountOption::AutoUnmount.render(), "auto_unmount");
        assert_eq!(MountOption::DefaultPermissions.render(), "default_permissions");
        assert_eq!(MountOption::FsName("myfs".to_string()).render(), "fsname=myfs");
        assert_eq!(MountOption::Subtype("fuse.myfs".to_string()).render(), "subtype=fuse.myfs");
        assert_eq!(MountOption::Custom("noatime".to_string()).render(), "noatime");
    }

    #[test]
    fn mount_option_rendering_joins_with_commas() {
        let options = [
            MountOption::ReadOnly,
            MountOption::FsName("myfs".to_string()),
            MountOption::Custom("noatime".to_string()),
        ];
        let joined = options
            .iter()
            .map(MountOption::render)
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(joined, "ro,fsname=myfs,noatime");
    }
}
