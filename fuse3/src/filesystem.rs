//! The `Filesystem` trait: the safe, synchronous, single-threaded interface
//! filesystem authors implement against.
//!
//! Every method has a default implementation so that a filesystem only has
//! to override the operations it actually supports. Fallible operations
//! default to `Err(Errno::ENOSYS)`; operations libfuse treats as
//! successful no-ops when the corresponding callback is left unset (init,
//! destroy, forget, open, opendir, release, releasedir, flush, fsync,
//! fsyncdir, access) default to their `Ok`/no-op equivalent here too.
//!
//! All methods take `&mut self`: the session driving a `Filesystem` runs a
//! single-threaded `fuse_session_loop`, so callbacks are never invoked
//! concurrently and no `Send`/`Sync` bound is required.

use std::borrow::Cow;
use std::time::Duration;

use crate::types::{
    ConnInfo, DirBuffer, DirPlusBuffer, Entry, Errno, FileAttr, FileInfo, Inode, OpenReply,
    Request, SetAttrs, StatFs,
};

/// The safe, low-level FUSE filesystem interface.
///
/// See the module documentation for the default-implementation policy.
#[allow(unused_variables)]
pub trait Filesystem {
    /// Called once when libfuse establishes communication with the kernel.
    /// `conn` may be inspected/adjusted (e.g. `max_write`, `max_readahead`).
    /// There is no reply.
    fn init(&mut self, conn: &mut ConnInfo) {}

    /// Called on filesystem exit, after the kernel connection may already
    /// be gone. There is no reply.
    fn destroy(&mut self) {}

    /// Looks up a directory entry by name in `parent` and returns its
    /// attributes.
    fn lookup(&mut self, req: &Request, parent: Inode, name: &str) -> Result<Entry, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Called when the kernel drops `nlookup` references to `ino` from its
    /// internal cache (also serves `forget_multi`, which calls this once
    /// per inode in the batch). There is no reply.
    fn forget(&mut self, ino: Inode, nlookup: u64) {}

    /// Gets the attributes of `ino`. `fh` is `Some` if the request arrived
    /// via an already-open file handle. Returns the attributes plus how
    /// long the kernel may cache them.
    fn getattr(
        &mut self,
        req: &Request,
        ino: Inode,
        fh: Option<u64>,
    ) -> Result<(FileAttr, Duration), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Sets attributes of `ino` as described by `attrs` (only the `Some`
    /// fields should be applied). `fh` is `Some` if this originated from an
    /// `ftruncate()` on an open file handle. Returns the resulting
    /// attributes plus their cache timeout.
    fn setattr(
        &mut self,
        req: &Request,
        ino: Inode,
        attrs: &SetAttrs,
        fh: Option<u64>,
    ) -> Result<(FileAttr, Duration), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Reads the target of the symbolic link `ino`.
    fn readlink(&mut self, req: &Request, ino: Inode) -> Result<String, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a non-directory, non-symlink filesystem node (regular file,
    /// device, fifo, or socket) named `name` in `parent`.
    fn mknod(
        &mut self,
        req: &Request,
        parent: Inode,
        name: &str,
        mode: u32,
        rdev: u32,
    ) -> Result<Entry, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a directory named `name` in `parent`.
    fn mkdir(&mut self, req: &Request, parent: Inode, name: &str, mode: u32) -> Result<Entry, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the (non-directory) entry `name` from `parent`.
    fn unlink(&mut self, req: &Request, parent: Inode, name: &str) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the empty directory `name` from `parent`.
    fn rmdir(&mut self, req: &Request, parent: Inode, name: &str) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a symbolic link named `name` in `parent`, pointing at
    /// `link`.
    fn symlink(
        &mut self,
        req: &Request,
        parent: Inode,
        name: &str,
        link: &str,
    ) -> Result<Entry, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Renames `name` in `parent` to `newname` in `newparent`. `flags` may
    /// contain `RENAME_EXCHANGE`/`RENAME_NOREPLACE` (see `rename(2)`).
    #[allow(clippy::too_many_arguments)]
    fn rename(
        &mut self,
        req: &Request,
        parent: Inode,
        name: &str,
        newparent: Inode,
        newname: &str,
        flags: u32,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a hard link to `ino` named `newname` in `newparent`.
    fn link(
        &mut self,
        req: &Request,
        ino: Inode,
        newparent: Inode,
        newname: &str,
    ) -> Result<Entry, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Opens `ino`. The returned [`OpenReply`] carries a file handle plus
    /// caching hints that are written back into the kernel's file info.
    fn open(&mut self, req: &Request, ino: Inode, fi: &FileInfo) -> Result<OpenReply, Errno> {
        Ok(OpenReply::new(0))
    }

    /// Reads up to `size` bytes from `ino` at `offset`. The returned
    /// [`Cow`] is truncated to `size` by the caller before being sent, so
    /// implementations may over-return trailing data.
    fn read(
        &mut self,
        req: &Request,
        ino: Inode,
        size: usize,
        offset: u64,
        fi: &FileInfo,
    ) -> Result<Cow<'_, [u8]>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Writes `data` to `ino` at `offset`, returning the number of bytes
    /// actually written.
    fn write(
        &mut self,
        req: &Request,
        ino: Inode,
        data: &[u8],
        offset: u64,
        fi: &FileInfo,
    ) -> Result<usize, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Called on each `close()` of an open file (there may be zero or many
    /// per `open`).
    fn flush(&mut self, req: &Request, ino: Inode, fi: &FileInfo) -> Result<(), Errno> {
        Ok(())
    }

    /// Called when the last reference to an open file is dropped.
    fn release(&mut self, req: &Request, ino: Inode, fi: &FileInfo) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes file contents (and metadata, unless `datasync` is set) to
    /// storage.
    fn fsync(&mut self, req: &Request, ino: Inode, datasync: bool, fi: &FileInfo) -> Result<(), Errno> {
        Ok(())
    }

    /// Opens the directory `ino`. Same contract as [`Filesystem::open`].
    fn opendir(&mut self, req: &Request, ino: Inode, fi: &FileInfo) -> Result<OpenReply, Errno> {
        Ok(OpenReply::new(0))
    }

    /// Reads directory entries into `buf`, starting at `offset`.
    ///
    /// Offset contract: implementors must begin emitting entries at
    /// `offset` (0 means "from the beginning") and pass an increasing
    /// `next_offset` to each [`DirBuffer::add`] call; the kernel will pass
    /// the last `next_offset` it received back in as `offset` on the next
    /// call for the same handle. Stop iterating as soon as `add` returns
    /// `false` (the buffer is full) - the kernel will call again with the
    /// appropriate offset to continue. An empty buffer (nothing added)
    /// signals end-of-stream.
    fn readdir(
        &mut self,
        req: &Request,
        ino: Inode,
        offset: u64,
        fh: u64,
        buf: &mut DirBuffer,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Like [`Filesystem::readdir`], but each entry carries full attributes
    /// via [`DirPlusBuffer::add`]. Same offset contract applies.
    fn readdirplus(
        &mut self,
        req: &Request,
        ino: Inode,
        offset: u64,
        fh: u64,
        buf: &mut DirPlusBuffer,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Releases a directory handle opened by [`Filesystem::opendir`].
    fn releasedir(&mut self, req: &Request, ino: Inode, fi: &FileInfo) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes directory contents (and metadata, unless `datasync` is set).
    fn fsyncdir(
        &mut self,
        req: &Request,
        ino: Inode,
        datasync: bool,
        fi: &FileInfo,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Returns filesystem-wide statistics. `ino` is a hint (zero means
    /// "undefined").
    ///
    /// Defaults to a minimal-but-valid `StatFs` (`bsize`/`namelen` set,
    /// everything else zero) rather than `ENOSYS`, mirroring libfuse's own
    /// behavior when `fuse_lowlevel_ops::statfs` is left `NULL` (`do_statfs`
    /// in `fuse_lowlevel.c` replies with a default `statvfs` in that case
    /// rather than an error - `statfs(2)`/`df` are expected to always
    /// succeed on a mounted filesystem).
    fn statfs(&mut self, req: &Request, ino: Inode) -> Result<StatFs, Errno> {
        Ok(StatFs {
            bsize: 512,
            namelen: 255,
            ..Default::default()
        })
    }

    /// Sets the extended attribute `name` on `ino` to `value`. `flags` are
    /// the raw `XATTR_CREATE`/`XATTR_REPLACE` bits from `setxattr(2)`.
    fn setxattr(
        &mut self,
        req: &Request,
        ino: Inode,
        name: &str,
        value: &[u8],
        flags: i32,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Returns the value of the extended attribute `name` on `ino`. The
    /// wrapper automatically implements the libfuse size-query protocol
    /// (an incoming `size` of zero asks for the value's length only) using
    /// the length of the returned `Vec`.
    fn getxattr(&mut self, req: &Request, ino: Inode, name: &str) -> Result<Vec<u8>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Returns the NUL-separated list of extended attribute names on
    /// `ino`, in the raw `listxattr(2)` wire format. Same size-query
    /// protocol as [`Filesystem::getxattr`].
    fn listxattr(&mut self, req: &Request, ino: Inode) -> Result<Vec<u8>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the extended attribute `name` from `ino`.
    fn removexattr(&mut self, req: &Request, ino: Inode, name: &str) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Checks whether the calling process may access `ino` per `mask`
    /// (the `access(2)`/`R_OK`/`W_OK`/`X_OK`/`F_OK` bits).
    fn access(&mut self, req: &Request, ino: Inode, mask: i32) -> Result<(), Errno> {
        Ok(())
    }

    /// Atomically creates and opens a regular file named `name` in
    /// `parent`.
    fn create(
        &mut self,
        req: &Request,
        parent: Inode,
        name: &str,
        mode: u32,
        fi: &FileInfo,
    ) -> Result<(Entry, OpenReply), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Pre-allocates (or deallocates/punches, depending on `mode` -  see
    /// `fallocate(2)`) `length` bytes at `offset` in `ino`.
    fn fallocate(
        &mut self,
        req: &Request,
        ino: Inode,
        mode: i32,
        offset: u64,
        length: u64,
        fi: &FileInfo,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Finds the next data region or hole at or after `offset` (see
    /// `lseek(2)`'s `SEEK_DATA`/`SEEK_HOLE`), returning the resulting
    /// offset.
    fn lseek(
        &mut self,
        req: &Request,
        ino: Inode,
        offset: u64,
        whence: i32,
        fi: &FileInfo,
    ) -> Result<u64, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Copies `len` bytes from `ino_in`@`off_in` to `ino_out`@`off_out`,
    /// returning the number of bytes actually copied.
    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &mut self,
        req: &Request,
        ino_in: Inode,
        off_in: u64,
        fi_in: &FileInfo,
        ino_out: Inode,
        off_out: u64,
        fi_out: &FileInfo,
        len: u64,
        flags: i32,
    ) -> Result<usize, Errno> {
        Err(Errno::ENOSYS)
    }
}
