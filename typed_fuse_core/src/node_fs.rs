//! The [`NodeFs`] trait and the value types it exchanges with the runtime.

use std::borrow::Cow;

use crate::attr::{FileKind, NodeAttr, SetAttr, StatFs};
use crate::errno::Errno;
use crate::runtime::Cx;

/// An opaque handle to a node tracked by the runtime. Numerically equal to
/// the FUSE inode number, but filesystems should treat it as opaque and
/// store these (rather than raw `u64`s) in their own directory structures.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId(u64);

impl NodeId {
    /// The filesystem root (`FUSE_ROOT_ID`).
    pub const ROOT: NodeId = NodeId(1);

    /// The underlying inode number.
    pub fn ino(self) -> u64 {
        self.0
    }

    pub(crate) fn from_ino(ino: u64) -> Self {
        NodeId(ino)
    }
}

/// Credentials of the process that issued the current request.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub umask: u32,
}

/// Connection parameters passed to [`NodeFs::init`], read from (and, for the
/// mutable fields, written back into) the kernel connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnInfo {
    pub proto_major: u32,
    pub proto_minor: u32,
    pub max_write: u32,
    pub max_readahead: u32,
}

/// Kernel caching hints returned alongside an opened handle.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenHints {
    pub direct_io: bool,
    pub keep_cache: bool,
    pub nonseekable: bool,
    pub cache_readdir: bool,
}

/// The result of an `open`/`opendir`/`create`: the filesystem's own handle
/// object plus optional caching hints. The runtime assigns the integer file
/// handle; the filesystem never sees it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opened<H> {
    pub handle: H,
    pub hints: OpenHints,
}

impl<H> Opened<H> {
    pub fn new(handle: H) -> Self {
        Opened {
            handle,
            hints: OpenHints::default(),
        }
    }

    pub fn direct_io(mut self, value: bool) -> Self {
        self.hints.direct_io = value;
        self
    }

    pub fn keep_cache(mut self, value: bool) -> Self {
        self.hints.keep_cache = value;
        self
    }

    pub fn nonseekable(mut self, value: bool) -> Self {
        self.hints.nonseekable = value;
        self
    }

    pub fn cache_readdir(mut self, value: bool) -> Self {
        self.hints.cache_readdir = value;
        self
    }
}

/// The reply to `getxattr`/`listxattr`. The kernel's xattr protocol is
/// two-phase (`size == 0` asks only for the length); [`XattrReply::Size`]
/// answers the length without materializing the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XattrReply {
    Size(usize),
    Data(Vec<u8>),
}

impl From<Vec<u8>> for XattrReply {
    fn from(data: Vec<u8>) -> Self {
        XattrReply::Data(data)
    }
}

/// A sink [`NodeFs::readdir`] pushes directory entries into. The runtime
/// (via `fuse3`) implements this over libfuse's size-limited buffer
/// protocol.
pub trait DirSink {
    /// Adds one entry. `next_offset` is the resume cookie the kernel will
    /// hand back to continue after this entry. Returns `false` once the
    /// buffer is full, at which point the caller must stop iterating.
    fn add(&mut self, name: &str, id: NodeId, kind: FileKind, next_offset: u64) -> bool;
}

/// The safe, node-based FUSE filesystem interface.
///
/// The runtime owns node identity, lifetime (lookup/link/open refcounts and
/// deferred deletion), and the file-handle table. Filesystem authors work
/// with their own [`NodeFs::Node`] and [`NodeFs::Handle`] payloads:
///
/// * Operations on a single existing node receive `&`/`&mut Self::Node`
///   directly (the runtime resolves it, replying `ENOENT` if it is gone).
/// * I/O operations additionally receive `&mut Self::Handle`.
/// * Structural / naming operations receive a [`Cx`] to resolve, insert, and
///   link other nodes.
///
/// Every method has a default (fallible ones default to `Err(ENOSYS)`;
/// no-op-style ones to `Ok`), so a filesystem overrides only what it
/// supports. All methods take `&mut self`: the driving session loop is
/// single-threaded.
#[allow(unused_variables)]
pub trait NodeFs: Sized {
    /// Per-node data owned and stored by the runtime.
    type Node;
    /// Per-open-file data. Use `()` if the filesystem is stateless per open.
    type Handle: Default;
    /// Per-open-directory data. Use `()` if not needed.
    type DirHandle: Default;

    /// Builds the payload for the root directory. Called once, when the
    /// runtime is constructed.
    fn root(&mut self) -> Self::Node;

    /// Populates the initial tree beneath the (already-inserted) root, for
    /// filesystems with statically-known contents. Called once, right after
    /// [`NodeFs::root`], with a [`Cx`] so children can be inserted and
    /// recorded. Default: no-op (an empty root).
    fn populate(&mut self, cx: &mut Cx<'_, Self::Node>) {}

    /// Called once when libfuse establishes communication with the kernel.
    fn init(&mut self, conn: &mut ConnInfo) {}

    /// Called on filesystem exit.
    fn destroy(&mut self) {}

    /// Returns the attributes of `node`, including its current link count.
    fn getattr(&mut self, node: &Self::Node, caller: &Caller) -> Result<NodeAttr, Errno>;

    /// Applies the `Some` fields of `set` to `node`, returning the resulting
    /// attributes.
    fn setattr(
        &mut self,
        node: &mut Self::Node,
        set: &SetAttr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Looks up `name` in directory `parent`. Return `Ok(Some(id))` for a
    /// hit, `Ok(None)` to populate the kernel's negative-lookup cache, or
    /// `Err` for a hard failure.
    fn lookup(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        parent: NodeId,
        name: &str,
        caller: &Caller,
    ) -> Result<Option<NodeId>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Reads the target of the symbolic link `node`.
    fn readlink(&mut self, node: &Self::Node, caller: &Caller) -> Result<String, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a non-directory, non-symlink node named `name` in `parent`.
    /// Insert it via `cx.insert(..)`, record its [`NodeId`] in the parent,
    /// and return the id.
    #[allow(clippy::too_many_arguments)]
    fn mknod(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        parent: NodeId,
        name: &str,
        mode: u32,
        rdev: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a directory named `name` in `parent`.
    fn mkdir(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        parent: NodeId,
        name: &str,
        mode: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a symbolic link named `name` in `parent` pointing at `target`.
    fn symlink(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        parent: NodeId,
        name: &str,
        target: &str,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the non-directory entry `name` from `parent`. Remove the name
    /// from the parent and call `cx.remove_link(id)`; the runtime frees the
    /// node once it is also un-looked-up and closed.
    fn unlink(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        parent: NodeId,
        name: &str,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the empty directory `name` from `parent`.
    fn rmdir(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        parent: NodeId,
        name: &str,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Renames `name` in `parent` to `newname` in `newparent`.
    #[allow(clippy::too_many_arguments)]
    fn rename(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        parent: NodeId,
        name: &str,
        newparent: NodeId,
        newname: &str,
        flags: u32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a hard link to `id` named `newname` in `newparent`. Record
    /// the name and call `cx.add_link(id)`.
    fn link(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        id: NodeId,
        newparent: NodeId,
        newname: &str,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Opens `node`, returning the filesystem's handle object.
    fn open(
        &mut self,
        node: &mut Self::Node,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<Self::Handle>, Errno> {
        Ok(Opened::new(Self::Handle::default()))
    }

    /// Reads up to `size` bytes from `node` at `offset`. The returned data
    /// may borrow from `self`/`node`/`handle` for a zero-copy reply (they all
    /// share the lifetime `'a`).
    fn read<'a>(
        &'a mut self,
        node: &'a mut Self::Node,
        handle: &'a mut Self::Handle,
        offset: u64,
        size: usize,
        caller: &Caller,
    ) -> Result<Cow<'a, [u8]>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Writes `data` to `node` at `offset`, returning the count written.
    fn write(
        &mut self,
        node: &mut Self::Node,
        handle: &mut Self::Handle,
        data: &[u8],
        offset: u64,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Called on each `close()` of an open file.
    fn flush(
        &mut self,
        node: &mut Self::Node,
        handle: &mut Self::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Called when the last reference to an open file is dropped; consumes
    /// the handle.
    fn release(
        &mut self,
        node: &mut Self::Node,
        handle: Self::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes file contents (and metadata unless `datasync`).
    fn fsync(
        &mut self,
        node: &mut Self::Node,
        handle: &mut Self::Handle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Opens the directory `node`.
    fn opendir(
        &mut self,
        node: &mut Self::Node,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<Self::DirHandle>, Errno> {
        Ok(Opened::new(Self::DirHandle::default()))
    }

    /// Emits the entries of directory `node` into `sink`, starting at
    /// `offset`. `this`/`parent` are provided so the filesystem can emit
    /// `.` and `..` correctly. Push each entry with a strictly increasing
    /// `next_offset` cookie and stop as soon as [`DirSink::add`] returns
    /// `false`.
    #[allow(clippy::too_many_arguments)]
    fn readdir(
        &mut self,
        node: &Self::Node,
        this: NodeId,
        parent: NodeId,
        handle: &mut Self::DirHandle,
        offset: u64,
        sink: &mut dyn DirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Releases a directory handle; consumes it.
    fn releasedir(
        &mut self,
        node: &mut Self::Node,
        handle: Self::DirHandle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes directory contents.
    fn fsyncdir(
        &mut self,
        node: &mut Self::Node,
        handle: &mut Self::DirHandle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Returns filesystem-wide statistics. Defaults to a minimal-but-valid
    /// value (as libfuse does when the callback is unset).
    fn statfs(&mut self, caller: &Caller) -> Result<StatFs, Errno> {
        Ok(StatFs {
            bsize: 512,
            namelen: 255,
            ..Default::default()
        })
    }

    /// Sets extended attribute `name` on `node`.
    fn setxattr(
        &mut self,
        node: &mut Self::Node,
        name: &str,
        value: &[u8],
        flags: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Returns extended attribute `name` on `node`. A `size` of zero is a
    /// length query (return [`XattrReply::Size`]).
    fn getxattr(
        &mut self,
        node: &Self::Node,
        name: &str,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Returns the NUL-separated extended attribute names on `node`. Same
    /// size-query protocol as [`NodeFs::getxattr`].
    fn listxattr(
        &mut self,
        node: &Self::Node,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes extended attribute `name` from `node`.
    fn removexattr(
        &mut self,
        node: &mut Self::Node,
        name: &str,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Checks access to `node` per the `access(2)` `mask`.
    fn access(&mut self, node: &Self::Node, mask: i32, caller: &Caller) -> Result<(), Errno> {
        Ok(())
    }

    /// Atomically creates and opens a regular file named `name` in `parent`.
    #[allow(clippy::too_many_arguments)]
    fn create(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        parent: NodeId,
        name: &str,
        mode: u32,
        umask: u32,
        flags: i32,
        caller: &Caller,
    ) -> Result<(NodeId, Opened<Self::Handle>), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Pre-allocates/punches `length` bytes at `offset` in `node`.
    fn fallocate(
        &mut self,
        node: &mut Self::Node,
        handle: &mut Self::Handle,
        mode: i32,
        offset: u64,
        length: u64,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Finds the next data region or hole at or after `offset`.
    fn lseek(
        &mut self,
        node: &mut Self::Node,
        handle: &mut Self::Handle,
        offset: u64,
        whence: i32,
        caller: &Caller,
    ) -> Result<u64, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Copies `len` bytes between two nodes (resolvable via `cx`).
    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &mut self,
        cx: &mut Cx<'_, Self::Node>,
        id_in: NodeId,
        off_in: u64,
        id_out: NodeId,
        off_out: u64,
        len: u64,
        flags: i32,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        Err(Errno::ENOSYS)
    }
}
