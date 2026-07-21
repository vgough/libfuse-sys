//! The node-tracking runtime: the base layer that owns inode identity,
//! lifetime (lookup/link/open refcounts and deferred deletion), and the
//! file-handle table, so filesystems don't have to.
//!
//! [`Runtime`] is generic, pure Rust, and C-free, so it is unit-testable
//! without mounting anything (see the tests at the bottom of this file).
//! `fuse3`'s trampolines decode raw C arguments, call a `Runtime` method,
//! and encode the returned core types back into `fuse_reply_*`.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::attr::{NodeAttr, SetAttr, StatFs};
use crate::errno::Errno;
use crate::node_fs::{Caller, ConnInfo, DirSink, NodeFs, NodeId, OpenHints, XattrReply};

// ---------------------------------------------------------------------
// Reply values handed back to the FFI layer
// ---------------------------------------------------------------------

/// An entry reply (`lookup`/`mknod`/`mkdir`/...): inode, generation, and
/// attributes, ready for `conv::entry_to_entry_param`.
#[derive(Clone, Copy, Debug)]
pub struct EntryReply {
    pub ino: u64,
    pub generation: u64,
    pub attr: NodeAttr,
}

/// The result of a `lookup`: a positive entry, or a negative-cache hit.
#[derive(Clone, Copy, Debug)]
pub enum LookupReply {
    Found(EntryReply),
    Negative,
}

/// The result of an `open`/`opendir`/`create`: the runtime-assigned file
/// handle plus the caching hints the filesystem requested.
#[derive(Clone, Copy, Debug)]
pub struct OpenReply {
    pub fh: u64,
    pub hints: OpenHints,
}

// ---------------------------------------------------------------------
// Node table
// ---------------------------------------------------------------------

struct Slot<N> {
    payload: N,
    generation: u64,
    /// Number of directory references (hard links). Together with
    /// `lookups`/`opens`, this drives deferred deletion. This is independent
    /// of the user-visible `NodeAttr::nlink`, which the filesystem supplies.
    links: u32,
    /// Kernel lookup count (incremented per entry reply, decremented by
    /// `forget`).
    lookups: u64,
    /// Number of currently-open handles referencing this node.
    opens: u32,
    /// Recorded parent, used to emit `..` in `readdir`.
    parent: NodeId,
}

impl<N> Slot<N> {
    fn is_droppable(&self) -> bool {
        self.links == 0 && self.lookups == 0 && self.opens == 0
    }
}

/// The inode table. Owns node payloads and assigns inode numbers +
/// generations, reclaiming (with a bumped generation) inodes whose nodes are
/// fully dropped.
pub struct NodeTable<N> {
    map: BTreeMap<u64, Slot<N>>,
    next_ino: u64,
    /// Reclaimed `(ino, next_generation)` pairs available for reuse.
    free: Vec<(u64, u64)>,
}

impl<N> NodeTable<N> {
    fn new() -> Self {
        NodeTable {
            map: BTreeMap::new(),
            next_ino: 2, // 1 is the root
            free: Vec::new(),
        }
    }

    fn alloc(&mut self) -> (u64, u64) {
        if let Some((ino, gen)) = self.free.pop() {
            (ino, gen)
        } else {
            let ino = self.next_ino;
            self.next_ino += 1;
            (ino, 0)
        }
    }

    /// Drops the node's payload and reclaims its inode iff nothing references
    /// it any more. The root (inode 1) keeps a permanent link and is never
    /// dropped.
    fn maybe_drop(&mut self, id: NodeId) {
        let ino = id.ino();
        let droppable = self.map.get(&ino).map(Slot::is_droppable).unwrap_or(false);
        if droppable {
            let gen = self.map.remove(&ino).unwrap().generation;
            self.free.push((ino, gen.wrapping_add(1)));
        }
    }
}

// ---------------------------------------------------------------------
// Cx: what structural ops use to resolve/insert/link nodes
// ---------------------------------------------------------------------

/// A view over the node table handed to structural [`NodeFs`] operations
/// (`lookup`, `mkdir`, `rename`, ...). Resolving a node here replaces the
/// per-filesystem inode map, and inserting/linking here drives the runtime's
/// identity allocation and lifetime tracking.
pub struct Cx<'a, N> {
    table: &'a mut NodeTable<N>,
}

impl<'a, N> Cx<'a, N> {
    /// Borrows the payload of `id`, if it exists.
    pub fn get(&self, id: NodeId) -> Option<&N> {
        self.table.map.get(&id.ino()).map(|s| &s.payload)
    }

    /// Mutably borrows the payload of `id`, if it exists.
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut N> {
        self.table.map.get_mut(&id.ino()).map(|s| &mut s.payload)
    }

    /// Mutably borrows two distinct nodes at once (e.g. for `rename`).
    /// Returns `None` if `a == b` or either is missing.
    pub fn get_disjoint_mut(&mut self, a: NodeId, b: NodeId) -> Option<(&mut N, &mut N)> {
        if a == b {
            return None;
        }
        let pa = self.table.map.get_mut(&a.ino())? as *mut Slot<N>;
        let pb = self.table.map.get_mut(&b.ino())? as *mut Slot<N>;
        // SAFETY: `a != b`, so these are distinct entries of the map; a
        // `BTreeMap` never hands out aliasing references for distinct keys,
        // and the two raw pointers were taken from separate lookups with no
        // live borrow in between.
        unsafe { Some((&mut (*pa).payload, &mut (*pb).payload)) }
    }

    /// Whether `id` is currently live.
    pub fn contains(&self, id: NodeId) -> bool {
        self.table.map.contains_key(&id.ino())
    }

    /// Inserts a brand-new node with `parent` as its recorded parent,
    /// returning its freshly assigned [`NodeId`]. The node starts with one
    /// directory link (the entry the caller is about to record in `parent`).
    pub fn insert(&mut self, payload: N, parent: NodeId) -> NodeId {
        let (ino, generation) = self.table.alloc();
        self.table.map.insert(
            ino,
            Slot {
                payload,
                generation,
                links: 1,
                lookups: 0,
                opens: 0,
                parent,
            },
        );
        NodeId::from_ino(ino)
    }

    /// Updates the recorded parent of `id` (used to emit `..` in `readdir`),
    /// e.g. after a directory is moved by `rename`.
    pub fn reparent(&mut self, id: NodeId, new_parent: NodeId) {
        if let Some(s) = self.table.map.get_mut(&id.ino()) {
            s.parent = new_parent;
        }
    }

    /// Records an additional directory reference to `id` (a hard link).
    pub fn add_link(&mut self, id: NodeId) {
        if let Some(s) = self.table.map.get_mut(&id.ino()) {
            s.links += 1;
        }
    }

    /// Drops a directory reference to `id`, freeing the node if it is now
    /// fully unreferenced (no links, no kernel lookups, no open handles).
    pub fn remove_link(&mut self, id: NodeId) {
        if let Some(s) = self.table.map.get_mut(&id.ino()) {
            s.links = s.links.saturating_sub(1);
        }
        self.table.maybe_drop(id);
    }
}

// ---------------------------------------------------------------------
// Handle table
// ---------------------------------------------------------------------

struct HandleTable<F: NodeFs> {
    files: BTreeMap<u64, F::Handle>,
    dirs: BTreeMap<u64, F::DirHandle>,
    next_fh: u64,
}

impl<F: NodeFs> HandleTable<F> {
    fn new() -> Self {
        HandleTable {
            files: BTreeMap::new(),
            dirs: BTreeMap::new(),
            next_fh: 1,
        }
    }

    fn alloc_file(&mut self, payload: F::Handle) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.files.insert(fh, payload);
        fh
    }

    fn alloc_dir(&mut self, payload: F::DirHandle) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.dirs.insert(fh, payload);
        fh
    }

    fn file_mut(&mut self, fh: u64) -> Option<&mut F::Handle> {
        self.files.get_mut(&fh)
    }

    fn dir_mut(&mut self, fh: u64) -> Option<&mut F::DirHandle> {
        self.dirs.get_mut(&fh)
    }

    fn remove_file(&mut self, fh: u64) -> Option<F::Handle> {
        self.files.remove(&fh)
    }

    fn remove_dir(&mut self, fh: u64) -> Option<F::DirHandle> {
        self.dirs.remove(&fh)
    }
}

// ---------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------

/// The node-tracking runtime wrapping a filesystem `F`. Owns the inode and
/// handle tables and mediates every operation.
pub struct Runtime<F: NodeFs> {
    fs: F,
    table: NodeTable<F::Node>,
    handles: HandleTable<F>,
    ttl: Duration,
    negative_ttl: Duration,
}

impl<F: NodeFs> Runtime<F> {
    /// Builds a runtime around `fs`, seeding the root node from
    /// [`NodeFs::root`].
    pub fn new(mut fs: F) -> Self {
        let root = fs.root();
        let mut table = NodeTable::new();
        table.map.insert(
            NodeId::ROOT.ino(),
            Slot {
                payload: root,
                generation: 0,
                links: 1,
                lookups: 0,
                opens: 0,
                parent: NodeId::ROOT,
            },
        );
        // Let the filesystem seed any statically-known children now that the
        // root exists.
        {
            let mut cx = Cx { table: &mut table };
            fs.populate(&mut cx);
        }
        Runtime {
            fs,
            table,
            handles: HandleTable::new(),
            ttl: Duration::from_secs(1),
            negative_ttl: Duration::from_secs(1),
        }
    }

    /// The entry/attribute cache TTL sent to the kernel.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// The negative-lookup cache TTL.
    pub fn negative_ttl(&self) -> Duration {
        self.negative_ttl
    }

    /// Sets the entry/attribute cache TTL.
    pub fn set_ttl(&mut self, ttl: Duration) {
        self.ttl = ttl;
    }

    /// Sets the negative-lookup cache TTL.
    pub fn set_negative_ttl(&mut self, ttl: Duration) {
        self.negative_ttl = ttl;
    }

    /// Builds an [`EntryReply`] for `id` and bumps the kernel lookup count
    /// (every entry reply hands the kernel a new reference that a later
    /// `forget` will release).
    fn entry_for(&mut self, id: NodeId, caller: &Caller) -> Result<EntryReply, Errno> {
        let Runtime { fs, table, .. } = self;
        let slot = table.map.get(&id.ino()).ok_or(Errno::ENOENT)?;
        let attr = fs.getattr(&slot.payload, caller)?;
        let generation = slot.generation;
        table.map.get_mut(&id.ino()).unwrap().lookups += 1;
        Ok(EntryReply {
            ino: id.ino(),
            generation,
            attr,
        })
    }

    // --- lifecycle / no-node ops ---

    pub fn init(&mut self, conn: &mut ConnInfo) {
        self.fs.init(conn);
    }

    pub fn destroy(&mut self) {
        self.fs.destroy();
    }

    pub fn forget(&mut self, ino: u64, nlookup: u64) {
        if let Some(s) = self.table.map.get_mut(&ino) {
            s.lookups = s.lookups.saturating_sub(nlookup);
        }
        self.table.maybe_drop(NodeId::from_ino(ino));
    }

    pub fn statfs(&mut self, _ino: u64, caller: &Caller) -> Result<StatFs, Errno> {
        self.fs.statfs(caller)
    }

    // --- single existing node ---

    pub fn getattr(&mut self, ino: u64, caller: &Caller) -> Result<NodeAttr, Errno> {
        let Runtime { fs, table, .. } = self;
        let slot = table.map.get(&ino).ok_or(Errno::ENOENT)?;
        fs.getattr(&slot.payload, caller)
    }

    pub fn setattr(
        &mut self,
        ino: u64,
        set: &SetAttr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        let Runtime { fs, table, .. } = self;
        let slot = table.map.get_mut(&ino).ok_or(Errno::ENOENT)?;
        fs.setattr(&mut slot.payload, set, caller)
    }

    pub fn readlink(&mut self, ino: u64, caller: &Caller) -> Result<String, Errno> {
        let Runtime { fs, table, .. } = self;
        let slot = table.map.get(&ino).ok_or(Errno::ENOENT)?;
        fs.readlink(&slot.payload, caller)
    }

    pub fn access(&mut self, ino: u64, mask: i32, caller: &Caller) -> Result<(), Errno> {
        let Runtime { fs, table, .. } = self;
        let slot = table.map.get(&ino).ok_or(Errno::ENOENT)?;
        fs.access(&slot.payload, mask, caller)
    }

    pub fn setxattr(
        &mut self,
        ino: u64,
        name: &str,
        value: &[u8],
        flags: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let Runtime { fs, table, .. } = self;
        let slot = table.map.get_mut(&ino).ok_or(Errno::ENOENT)?;
        fs.setxattr(&mut slot.payload, name, value, flags, caller)
    }

    pub fn getxattr(
        &mut self,
        ino: u64,
        name: &str,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        let Runtime { fs, table, .. } = self;
        let slot = table.map.get(&ino).ok_or(Errno::ENOENT)?;
        fs.getxattr(&slot.payload, name, size, caller)
    }

    pub fn listxattr(
        &mut self,
        ino: u64,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        let Runtime { fs, table, .. } = self;
        let slot = table.map.get(&ino).ok_or(Errno::ENOENT)?;
        fs.listxattr(&slot.payload, size, caller)
    }

    pub fn removexattr(&mut self, ino: u64, name: &str, caller: &Caller) -> Result<(), Errno> {
        let Runtime { fs, table, .. } = self;
        let slot = table.map.get_mut(&ino).ok_or(Errno::ENOENT)?;
        fs.removexattr(&mut slot.payload, name, caller)
    }

    // --- open files ---

    pub fn open(&mut self, ino: u64, flags: i32, caller: &Caller) -> Result<OpenReply, Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let slot = table.map.get_mut(&ino).ok_or(Errno::ENOENT)?;
        let opened = fs.open(&mut slot.payload, flags, caller)?;
        let fh = handles.alloc_file(opened.handle);
        slot.opens += 1;
        Ok(OpenReply {
            fh,
            hints: opened.hints,
        })
    }

    pub fn read(
        &mut self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: usize,
        caller: &Caller,
    ) -> Result<Cow<'_, [u8]>, Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let node = table
            .map
            .get_mut(&ino)
            .map(|s| &mut s.payload)
            .ok_or(Errno::ENOENT)?;
        let handle = handles.file_mut(fh).ok_or(Errno::EBADF)?;
        fs.read(node, handle, offset, size, caller)
    }

    pub fn write(
        &mut self,
        ino: u64,
        fh: u64,
        data: &[u8],
        offset: u64,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let node = table
            .map
            .get_mut(&ino)
            .map(|s| &mut s.payload)
            .ok_or(Errno::ENOENT)?;
        let handle = handles.file_mut(fh).ok_or(Errno::EBADF)?;
        fs.write(node, handle, data, offset, caller)
    }

    pub fn flush(&mut self, ino: u64, fh: u64, caller: &Caller) -> Result<(), Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let node = table
            .map
            .get_mut(&ino)
            .map(|s| &mut s.payload)
            .ok_or(Errno::ENOENT)?;
        let handle = handles.file_mut(fh).ok_or(Errno::EBADF)?;
        fs.flush(node, handle, caller)
    }

    pub fn fsync(
        &mut self,
        ino: u64,
        fh: u64,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let node = table
            .map
            .get_mut(&ino)
            .map(|s| &mut s.payload)
            .ok_or(Errno::ENOENT)?;
        let handle = handles.file_mut(fh).ok_or(Errno::EBADF)?;
        fs.fsync(node, handle, datasync, caller)
    }

    pub fn fallocate(
        &mut self,
        ino: u64,
        fh: u64,
        mode: i32,
        offset: u64,
        length: u64,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let node = table
            .map
            .get_mut(&ino)
            .map(|s| &mut s.payload)
            .ok_or(Errno::ENOENT)?;
        let handle = handles.file_mut(fh).ok_or(Errno::EBADF)?;
        fs.fallocate(node, handle, mode, offset, length, caller)
    }

    pub fn lseek(
        &mut self,
        ino: u64,
        fh: u64,
        offset: u64,
        whence: i32,
        caller: &Caller,
    ) -> Result<u64, Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let node = table
            .map
            .get_mut(&ino)
            .map(|s| &mut s.payload)
            .ok_or(Errno::ENOENT)?;
        let handle = handles.file_mut(fh).ok_or(Errno::EBADF)?;
        fs.lseek(node, handle, offset, whence, caller)
    }

    pub fn release(&mut self, ino: u64, fh: u64, caller: &Caller) -> Result<(), Errno> {
        let handle = self.handles.remove_file(fh).ok_or(Errno::EBADF)?;
        let res = {
            let Runtime { fs, table, .. } = self;
            let slot = table.map.get_mut(&ino).ok_or(Errno::ENOENT)?;
            slot.opens = slot.opens.saturating_sub(1);
            fs.release(&mut slot.payload, handle, caller)
        };
        self.table.maybe_drop(NodeId::from_ino(ino));
        res
    }

    // --- open dirs ---

    pub fn opendir(&mut self, ino: u64, flags: i32, caller: &Caller) -> Result<OpenReply, Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let slot = table.map.get_mut(&ino).ok_or(Errno::ENOENT)?;
        let opened = fs.opendir(&mut slot.payload, flags, caller)?;
        let fh = handles.alloc_dir(opened.handle);
        slot.opens += 1;
        Ok(OpenReply {
            fh,
            hints: opened.hints,
        })
    }

    pub fn readdir(
        &mut self,
        ino: u64,
        fh: u64,
        offset: u64,
        sink: &mut dyn DirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let slot = table.map.get(&ino).ok_or(Errno::ENOENT)?;
        let parent = slot.parent;
        // Directories are normally opened first, but be lenient if no handle
        // is present.
        let mut tmp = <F::DirHandle as Default>::default();
        let handle = match handles.dir_mut(fh) {
            Some(h) => h,
            None => &mut tmp,
        };
        fs.readdir(
            &slot.payload,
            NodeId::from_ino(ino),
            parent,
            handle,
            offset,
            sink,
            caller,
        )
    }

    pub fn fsyncdir(
        &mut self,
        ino: u64,
        fh: u64,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let Runtime {
            fs, table, handles, ..
        } = self;
        let node = table
            .map
            .get_mut(&ino)
            .map(|s| &mut s.payload)
            .ok_or(Errno::ENOENT)?;
        let mut tmp = <F::DirHandle as Default>::default();
        let handle = match handles.dir_mut(fh) {
            Some(h) => h,
            None => &mut tmp,
        };
        fs.fsyncdir(node, handle, datasync, caller)
    }

    pub fn releasedir(&mut self, ino: u64, fh: u64, caller: &Caller) -> Result<(), Errno> {
        let handle = self.handles.remove_dir(fh).unwrap_or_default();
        let res = {
            let Runtime { fs, table, .. } = self;
            let slot = table.map.get_mut(&ino).ok_or(Errno::ENOENT)?;
            slot.opens = slot.opens.saturating_sub(1);
            fs.releasedir(&mut slot.payload, handle, caller)
        };
        self.table.maybe_drop(NodeId::from_ino(ino));
        res
    }

    // --- structural / naming ---

    pub fn lookup(
        &mut self,
        parent: u64,
        name: &str,
        caller: &Caller,
    ) -> Result<LookupReply, Errno> {
        let found = {
            let Runtime { fs, table, .. } = self;
            let mut cx = Cx { table };
            fs.lookup(&mut cx, NodeId::from_ino(parent), name, caller)?
        };
        match found {
            Some(id) => Ok(LookupReply::Found(self.entry_for(id, caller)?)),
            None => Ok(LookupReply::Negative),
        }
    }

    pub fn mknod(
        &mut self,
        parent: u64,
        name: &str,
        mode: u32,
        rdev: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = {
            let Runtime { fs, table, .. } = self;
            let mut cx = Cx { table };
            fs.mknod(&mut cx, NodeId::from_ino(parent), name, mode, rdev, umask, caller)?
        };
        self.entry_for(id, caller)
    }

    pub fn mkdir(
        &mut self,
        parent: u64,
        name: &str,
        mode: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = {
            let Runtime { fs, table, .. } = self;
            let mut cx = Cx { table };
            fs.mkdir(&mut cx, NodeId::from_ino(parent), name, mode, umask, caller)?
        };
        self.entry_for(id, caller)
    }

    pub fn symlink(
        &mut self,
        parent: u64,
        name: &str,
        target: &str,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = {
            let Runtime { fs, table, .. } = self;
            let mut cx = Cx { table };
            fs.symlink(&mut cx, NodeId::from_ino(parent), name, target, caller)?
        };
        self.entry_for(id, caller)
    }

    pub fn link(
        &mut self,
        ino: u64,
        newparent: u64,
        newname: &str,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = {
            let Runtime { fs, table, .. } = self;
            let mut cx = Cx { table };
            fs.link(
                &mut cx,
                NodeId::from_ino(ino),
                NodeId::from_ino(newparent),
                newname,
                caller,
            )?
        };
        self.entry_for(id, caller)
    }

    pub fn unlink(&mut self, parent: u64, name: &str, caller: &Caller) -> Result<(), Errno> {
        let Runtime { fs, table, .. } = self;
        let mut cx = Cx { table };
        fs.unlink(&mut cx, NodeId::from_ino(parent), name, caller)
    }

    pub fn rmdir(&mut self, parent: u64, name: &str, caller: &Caller) -> Result<(), Errno> {
        let Runtime { fs, table, .. } = self;
        let mut cx = Cx { table };
        fs.rmdir(&mut cx, NodeId::from_ino(parent), name, caller)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &mut self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        flags: u32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let Runtime { fs, table, .. } = self;
        let mut cx = Cx { table };
        fs.rename(
            &mut cx,
            NodeId::from_ino(parent),
            name,
            NodeId::from_ino(newparent),
            newname,
            flags,
            caller,
        )
    }

    pub fn create(
        &mut self,
        parent: u64,
        name: &str,
        mode: u32,
        umask: u32,
        flags: i32,
        caller: &Caller,
    ) -> Result<(EntryReply, OpenReply), Errno> {
        let (id, opened) = {
            let Runtime { fs, table, .. } = self;
            let mut cx = Cx { table };
            fs.create(&mut cx, NodeId::from_ino(parent), name, mode, umask, flags, caller)?
        };
        let entry = self.entry_for(id, caller)?;
        let fh = self.handles.alloc_file(opened.handle);
        self.table.map.get_mut(&id.ino()).unwrap().opens += 1;
        Ok((
            entry,
            OpenReply {
                fh,
                hints: opened.hints,
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn copy_file_range(
        &mut self,
        ino_in: u64,
        off_in: u64,
        ino_out: u64,
        off_out: u64,
        len: u64,
        flags: i32,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        let Runtime { fs, table, .. } = self;
        let mut cx = Cx { table };
        fs.copy_file_range(
            &mut cx,
            NodeId::from_ino(ino_in),
            off_in,
            NodeId::from_ino(ino_out),
            off_out,
            len,
            flags,
            caller,
        )
    }
}

// ---------------------------------------------------------------------
// Tests: the runtime is fully exercisable without any FUSE mount.
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr::FileKind;
    use std::collections::BTreeMap;

    enum Data {
        Dir(BTreeMap<String, NodeId>),
        File(Vec<u8>),
    }

    struct Node {
        data: Data,
    }

    #[derive(Default)]
    struct Mini;

    impl Node {
        fn dir() -> Node {
            Node {
                data: Data::Dir(BTreeMap::new()),
            }
        }
        fn file() -> Node {
            Node {
                data: Data::File(Vec::new()),
            }
        }
    }

    impl NodeFs for Mini {
        type Node = Node;
        type Handle = ();
        type DirHandle = ();

        fn root(&mut self) -> Node {
            Node::dir()
        }

        fn getattr(&mut self, node: &Node, _c: &Caller) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr {
                kind: match node.data {
                    Data::Dir(_) => FileKind::Directory,
                    Data::File(_) => FileKind::RegularFile,
                },
                perm: 0o644,
                nlink: match node.data {
                    // Deliberately distinctive so the test below catches
                    // any runtime-side replacement of this value.
                    Data::Dir(_) => 7,
                    Data::File(_) => 1,
                },
                ..Default::default()
            })
        }

        fn lookup(
            &mut self,
            cx: &mut Cx<'_, Node>,
            parent: NodeId,
            name: &str,
            _c: &Caller,
        ) -> Result<Option<NodeId>, Errno> {
            let p = cx.get(parent).ok_or(Errno::ENOENT)?;
            match &p.data {
                Data::Dir(entries) => Ok(entries.get(name).copied()),
                _ => Err(Errno::ENOTDIR),
            }
        }

        fn create(
            &mut self,
            cx: &mut Cx<'_, Node>,
            parent: NodeId,
            name: &str,
            _mode: u32,
            _umask: u32,
            _flags: i32,
            _c: &Caller,
        ) -> Result<(NodeId, Opened<()>), Errno> {
            let id = cx.insert(Node::file(), parent);
            if let Some(Data::Dir(entries)) = cx.get_mut(parent).map(|n| &mut n.data) {
                entries.insert(name.to_string(), id);
            }
            Ok((id, Opened::new(())))
        }

        fn unlink(
            &mut self,
            cx: &mut Cx<'_, Node>,
            parent: NodeId,
            name: &str,
            _c: &Caller,
        ) -> Result<(), Errno> {
            let id = match cx.get_mut(parent).map(|n| &mut n.data) {
                Some(Data::Dir(entries)) => entries.remove(name).ok_or(Errno::ENOENT)?,
                _ => return Err(Errno::ENOTDIR),
            };
            cx.remove_link(id);
            Ok(())
        }

        fn read<'a>(
            &'a mut self,
            node: &'a mut Node,
            _h: &'a mut (),
            offset: u64,
            size: usize,
            _c: &Caller,
        ) -> Result<Cow<'a, [u8]>, Errno> {
            if let Data::File(content) = &node.data {
                let off = offset as usize;
                if off >= content.len() {
                    return Ok(Cow::Borrowed(&[]));
                }
                let end = (off + size).min(content.len());
                Ok(Cow::Borrowed(&content[off..end]))
            } else {
                Err(Errno::EISDIR)
            }
        }
    }

    use crate::node_fs::Opened;

    fn caller() -> Caller {
        Caller::default()
    }

    // Test accessors.
    impl<F: NodeFs> Runtime<F> {
        fn node_count(&self) -> usize {
            self.table.map.len()
        }
        fn counts(&self, ino: u64) -> Option<(u32, u64, u32)> {
            self.table
                .map
                .get(&ino)
                .map(|s| (s.links, s.lookups, s.opens))
        }
        fn generation(&self, ino: u64) -> Option<u64> {
            self.table.map.get(&ino).map(|s| s.generation)
        }
    }

    #[test]
    fn root_seeded() {
        let rt = Runtime::new(Mini);
        assert_eq!(rt.node_count(), 1);
        assert_eq!(rt.counts(NodeId::ROOT.ino()), Some((1, 0, 0)));
    }

    #[test]
    fn create_allocates_inode_and_counts_lookup() {
        let mut rt = Runtime::new(Mini);
        let (entry, _open) = rt
            .create(NodeId::ROOT.ino(), "f", 0o644, 0, 0, &caller())
            .unwrap();
        assert_eq!(entry.ino, 2);
        // one directory link, one kernel lookup (from the entry reply), one
        // open handle (create opens the file).
        assert_eq!(rt.counts(2), Some((1, 1, 1)));
    }

    #[test]
    fn getattr_preserves_filesystem_nlink() {
        let mut rt = Runtime::new(Mini);
        let attr = rt.getattr(NodeId::ROOT.ino(), &caller()).unwrap();
        assert_eq!(attr.nlink, 7);
    }

    #[test]
    fn unlink_while_open_defers_drop() {
        let mut rt = Runtime::new(Mini);
        let (entry, open) = rt
            .create(NodeId::ROOT.ino(), "f", 0o644, 0, 0, &caller())
            .unwrap();
        let ino = entry.ino;

        // unlink: link count drops to 0, but the node is still looked-up and
        // open, so it must survive.
        rt.unlink(NodeId::ROOT.ino(), "f", &caller()).unwrap();
        assert_eq!(rt.counts(ino), Some((0, 1, 1)));

        // reading through the open handle still resolves the node (it would
        // be ENOENT if the node had been dropped on unlink).
        assert!(rt.read(ino, open.fh, 0, 10, &caller()).is_ok());

        // forget the kernel reference: still open, still alive.
        rt.forget(ino, 1);
        assert_eq!(rt.counts(ino), Some((0, 0, 1)));

        // close: now fully unreferenced -> dropped, inode reclaimed.
        rt.release(ino, open.fh, &caller()).unwrap();
        assert_eq!(rt.counts(ino), None);
    }

    #[test]
    fn inode_reused_with_bumped_generation() {
        let mut rt = Runtime::new(Mini);
        let (entry, open) = rt
            .create(NodeId::ROOT.ino(), "f", 0o644, 0, 0, &caller())
            .unwrap();
        let ino = entry.ino;
        assert_eq!(rt.generation(ino), Some(0));

        rt.unlink(NodeId::ROOT.ino(), "f", &caller()).unwrap();
        rt.forget(ino, 1);
        rt.release(ino, open.fh, &caller()).unwrap();
        assert_eq!(rt.counts(ino), None);

        // Next create should reuse the inode with generation bumped to 1.
        let (entry2, _open2) = rt
            .create(NodeId::ROOT.ino(), "g", 0o644, 0, 0, &caller())
            .unwrap();
        assert_eq!(entry2.ino, ino);
        assert_eq!(rt.generation(ino), Some(1));
    }

    #[test]
    fn forget_saturates_and_missing_node_is_noop() {
        let mut rt = Runtime::new(Mini);
        // forgetting an unknown inode does nothing and does not panic.
        rt.forget(999, 5);
        // over-forgetting saturates rather than underflowing.
        let (entry, open) = rt
            .create(NodeId::ROOT.ino(), "f", 0o644, 0, 0, &caller())
            .unwrap();
        rt.forget(entry.ino, 100);
        assert_eq!(rt.counts(entry.ino), Some((1, 0, 1)));
        rt.release(entry.ino, open.fh, &caller()).unwrap();
    }

    #[test]
    fn read_missing_handle_is_ebadf() {
        let mut rt = Runtime::new(Mini);
        let (entry, _open) = rt
            .create(NodeId::ROOT.ino(), "f", 0o644, 0, 0, &caller())
            .unwrap();
        assert_eq!(
            rt.read(entry.ino, 4242, 0, 10, &caller()).unwrap_err(),
            Errno::EBADF
        );
    }
}
