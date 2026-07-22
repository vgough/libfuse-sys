//! Concurrent node and handle lifetime management for [`NodeFs`].

use std::borrow::Cow;
use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::attr::{NodeAttr, SetAttr, StatFs};
use crate::errno::Errno;
use crate::node_fs::{
    Caller, ConnInfo, ConnectionCapability, DirSink, NodeFs, NodeId, OpenHints, XattrReply,
};

#[derive(Clone, Copy, Debug)]
pub struct EntryReply {
    pub ino: u64,
    pub generation: u64,
    pub attr: NodeAttr,
}

#[derive(Clone, Copy, Debug)]
pub enum LookupReply {
    Found(EntryReply),
    Negative,
}

#[derive(Clone, Copy, Debug)]
pub struct OpenReply {
    pub fh: u64,
    pub hints: OpenHints,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct NodeRecord<N> {
    payload: N,
}

struct Slot<N> {
    record: Arc<NodeRecord<N>>,
    generation: u64,
    links: u32,
    lookups: u64,
    opens: u32,
    leases: u32,
    parent: NodeId,
}

impl<N> Slot<N> {
    fn droppable(&self) -> bool {
        self.links == 0 && self.lookups == 0 && self.opens == 0 && self.leases == 0
    }
}

/// Inode allocation and lifetime metadata. It is synchronized internally by
/// [`Runtime`]; the type is public only for compatibility with earlier
/// versions and is not normally used directly.
pub struct NodeTable<N> {
    map: BTreeMap<u64, Slot<N>>,
    next_ino: u64,
    free: Vec<(u64, u64)>,
}

impl<N> NodeTable<N> {
    fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            next_ino: 2,
            free: Vec::new(),
        }
    }

    fn alloc(&mut self) -> (u64, u64) {
        self.free.pop().unwrap_or_else(|| {
            let ino = self.next_ino;
            self.next_ino += 1;
            (ino, 0)
        })
    }

    fn maybe_drop(&mut self, id: NodeId) {
        let ino = id.ino();
        if self.map.get(&ino).is_some_and(Slot::droppable) {
            let generation = self.map.remove(&ino).unwrap().generation;
            self.free.push((ino, generation.wrapping_add(1)));
        }
    }
}

struct Shared<N> {
    table: Mutex<NodeTable<N>>,
}

/// A shared node lease. Keeping this value alive prevents the inode from
/// being reclaimed or reused. It dereferences to the filesystem's node.
pub struct NodeRef<N> {
    id: NodeId,
    record: Arc<NodeRecord<N>>,
    shared: Arc<Shared<N>>,
}

impl<N> Deref for NodeRef<N> {
    type Target = N;
    fn deref(&self) -> &N {
        &self.record.payload
    }
}

impl<N> Drop for NodeRef<N> {
    fn drop(&mut self) {
        let mut table = lock(&self.shared.table);
        if let Some(slot) = table.map.get_mut(&self.id.ino()) {
            if Arc::ptr_eq(&slot.record, &self.record) {
                slot.leases = slot.leases.saturating_sub(1);
            }
        }
        table.maybe_drop(self.id);
    }
}

/// Concurrent access to the runtime's node table for structural callbacks.
/// All mutation methods take `&self` and hold the metadata lock only for the
/// bookkeeping operation itself.
pub struct Cx<'a, N> {
    shared: &'a Arc<Shared<N>>,
}

impl<'a, N> Cx<'a, N> {
    pub fn get(&self, id: NodeId) -> Option<NodeRef<N>> {
        let mut table = lock(&self.shared.table);
        let slot = table.map.get_mut(&id.ino())?;
        slot.leases = slot
            .leases
            .checked_add(1)
            .expect("node lease count overflow");
        Some(NodeRef {
            id,
            record: Arc::clone(&slot.record),
            shared: Arc::clone(self.shared),
        })
    }

    pub fn contains(&self, id: NodeId) -> bool {
        lock(&self.shared.table).map.contains_key(&id.ino())
    }

    pub fn insert(&self, payload: N, parent: NodeId) -> NodeId {
        let mut table = lock(&self.shared.table);
        let (ino, generation) = table.alloc();
        table.map.insert(
            ino,
            Slot {
                record: Arc::new(NodeRecord { payload }),
                generation,
                links: 1,
                lookups: 0,
                opens: 0,
                leases: 0,
                parent,
            },
        );
        NodeId::from_ino(ino)
    }

    pub fn reparent(&self, id: NodeId, new_parent: NodeId) {
        if let Some(slot) = lock(&self.shared.table).map.get_mut(&id.ino()) {
            slot.parent = new_parent;
        }
    }

    pub fn add_link(&self, id: NodeId) {
        if let Some(slot) = lock(&self.shared.table).map.get_mut(&id.ino()) {
            slot.links = slot.links.checked_add(1).expect("node link count overflow");
        }
    }

    pub fn remove_link(&self, id: NodeId) {
        let mut table = lock(&self.shared.table);
        if let Some(slot) = table.map.get_mut(&id.ino()) {
            slot.links = slot.links.saturating_sub(1);
        }
        table.maybe_drop(id);
    }
}

struct HandleState {
    active: u32,
    closing: bool,
    taken: bool,
}

struct HandleRecord<H> {
    payload: UnsafeCell<ManuallyDrop<H>>,
    state: Mutex<HandleState>,
    drained: Condvar,
}

// The payload is only moved after closing prevents new leases and all prior
// leases have drained. Shared access is valid because `H: Sync`.
unsafe impl<H: Send + Sync> Send for HandleRecord<H> {}
unsafe impl<H: Send + Sync> Sync for HandleRecord<H> {}

impl<H> HandleRecord<H> {
    fn new(payload: H) -> Self {
        Self {
            payload: UnsafeCell::new(ManuallyDrop::new(payload)),
            state: Mutex::new(HandleState {
                active: 0,
                closing: false,
                taken: false,
            }),
            drained: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> Option<HandleLease<H>> {
        let mut state = lock(&self.state);
        if state.closing {
            return None;
        }
        state.active = state
            .active
            .checked_add(1)
            .expect("handle lease count overflow");
        drop(state);
        Some(HandleLease {
            record: Arc::clone(self),
        })
    }

    fn close_and_take(&self) -> H {
        let mut state = lock(&self.state);
        state.closing = true;
        while state.active != 0 {
            state = self.drained.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        state.taken = true;
        drop(state);
        // SAFETY: the record is closed, no leases exist, and this method is
        // called only by the thread that removed the record from the table.
        unsafe { ManuallyDrop::take(&mut *self.payload.get()) }
    }
}

impl<H> Drop for HandleRecord<H> {
    fn drop(&mut self) {
        if !lock(&self.state).taken {
            // SAFETY: `drop` has exclusive access to the record, so no lease
            // can still refer to the payload.
            unsafe { ManuallyDrop::drop(&mut *self.payload.get()) };
        }
    }
}

struct HandleLease<H> {
    record: Arc<HandleRecord<H>>,
}

impl<H> Deref for HandleLease<H> {
    type Target = H;
    fn deref(&self) -> &H {
        // SAFETY: acquiring the lease increments `active`; close waits for it.
        unsafe { &*self.record.payload.get() }
    }
}

impl<H> Drop for HandleLease<H> {
    fn drop(&mut self) {
        let mut state = lock(&self.record.state);
        state.active = state.active.saturating_sub(1);
        if state.active == 0 && state.closing {
            self.record.drained.notify_all();
        }
    }
}

struct HandleTable<F: NodeFs> {
    files: BTreeMap<u64, Arc<HandleRecord<F::Handle>>>,
    dirs: BTreeMap<u64, Arc<HandleRecord<F::DirHandle>>>,
    next_fh: u64,
}

impl<F: NodeFs> HandleTable<F> {
    fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            dirs: BTreeMap::new(),
            next_fh: 1,
        }
    }
    fn next(&mut self) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        fh
    }
    fn add_file(&mut self, handle: F::Handle) -> u64 {
        let fh = self.next();
        self.files.insert(fh, Arc::new(HandleRecord::new(handle)));
        fh
    }
    fn add_dir(&mut self, handle: F::DirHandle) -> u64 {
        let fh = self.next();
        self.dirs.insert(fh, Arc::new(HandleRecord::new(handle)));
        fh
    }
}

pub struct Runtime<F: NodeFs> {
    fs: F,
    shared: Arc<Shared<F::Node>>,
    handles: Mutex<HandleTable<F>>,
    ttl: Duration,
    negative_ttl: Duration,
    parallel_dirops: bool,
}

impl<F: NodeFs> Runtime<F> {
    pub fn new(mut fs: F) -> Self {
        let root = fs.root();
        let mut table = NodeTable::new();
        table.map.insert(
            1,
            Slot {
                record: Arc::new(NodeRecord { payload: root }),
                generation: 0,
                links: 1,
                lookups: 0,
                opens: 0,
                leases: 0,
                parent: NodeId::ROOT,
            },
        );
        let shared = Arc::new(Shared {
            table: Mutex::new(table),
        });
        {
            let cx = Cx { shared: &shared };
            fs.populate(&cx);
        }
        Self {
            fs,
            shared,
            handles: Mutex::new(HandleTable::new()),
            ttl: Duration::from_secs(1),
            negative_ttl: Duration::from_secs(1),
            parallel_dirops: false,
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }
    pub fn negative_ttl(&self) -> Duration {
        self.negative_ttl
    }
    pub fn set_ttl(&mut self, ttl: Duration) {
        self.ttl = ttl;
    }
    pub fn set_negative_ttl(&mut self, ttl: Duration) {
        self.negative_ttl = ttl;
    }
    #[doc(hidden)]
    pub fn set_parallel_dirops(&mut self, enabled: bool) {
        self.parallel_dirops = enabled;
    }
    fn cx(&self) -> Cx<'_, F::Node> {
        Cx {
            shared: &self.shared,
        }
    }
    fn node(&self, ino: u64) -> Result<NodeRef<F::Node>, Errno> {
        self.cx().get(NodeId::from_ino(ino)).ok_or(Errno::ENOENT)
    }

    fn entry_for(&self, id: NodeId, caller: &Caller) -> Result<EntryReply, Errno> {
        let node = self.cx().get(id).ok_or(Errno::ENOENT)?;
        let attr = self.fs.getattr(&node, caller)?;
        let mut table = lock(&self.shared.table);
        let slot = table.map.get_mut(&id.ino()).ok_or(Errno::ENOENT)?;
        let generation = slot.generation;
        slot.lookups = slot.lookups.checked_add(1).expect("lookup count overflow");
        Ok(EntryReply {
            ino: id.ino(),
            generation,
            attr,
        })
    }

    pub fn init(&self, conn: &mut ConnInfo) {
        if self.parallel_dirops {
            conn.set_enabled(ConnectionCapability::ParallelDirectoryOperations, true);
        }
        self.fs.init(conn);
    }
    pub fn destroy(&self) {
        self.fs.destroy();
    }
    pub fn forget(&self, ino: u64, nlookup: u64) {
        let mut t = lock(&self.shared.table);
        if let Some(s) = t.map.get_mut(&ino) {
            s.lookups = s.lookups.saturating_sub(nlookup);
        }
        t.maybe_drop(NodeId::from_ino(ino));
    }
    pub fn statfs(&self, _ino: u64, caller: &Caller) -> Result<StatFs, Errno> {
        self.fs.statfs(caller)
    }
    pub fn getattr(&self, ino: u64, caller: &Caller) -> Result<NodeAttr, Errno> {
        let n = self.node(ino)?;
        self.fs.getattr(&n, caller)
    }
    pub fn setattr(&self, ino: u64, set: &SetAttr, caller: &Caller) -> Result<NodeAttr, Errno> {
        let n = self.node(ino)?;
        self.fs.setattr(&n, set, caller)
    }
    pub fn readlink(&self, ino: u64, caller: &Caller) -> Result<String, Errno> {
        let n = self.node(ino)?;
        self.fs.readlink(&n, caller)
    }
    pub fn access(&self, ino: u64, mask: i32, caller: &Caller) -> Result<(), Errno> {
        let n = self.node(ino)?;
        self.fs.access(&n, mask, caller)
    }
    pub fn setxattr(
        &self,
        ino: u64,
        name: &str,
        value: &[u8],
        flags: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let n = self.node(ino)?;
        self.fs.setxattr(&n, name, value, flags, caller)
    }
    pub fn getxattr(
        &self,
        ino: u64,
        name: &str,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        let n = self.node(ino)?;
        self.fs.getxattr(&n, name, size, caller)
    }
    pub fn listxattr(&self, ino: u64, size: usize, caller: &Caller) -> Result<XattrReply, Errno> {
        let n = self.node(ino)?;
        self.fs.listxattr(&n, size, caller)
    }
    pub fn removexattr(&self, ino: u64, name: &str, caller: &Caller) -> Result<(), Errno> {
        let n = self.node(ino)?;
        self.fs.removexattr(&n, name, caller)
    }

    fn add_open(&self, ino: u64) -> Result<(), Errno> {
        let mut t = lock(&self.shared.table);
        let s = t.map.get_mut(&ino).ok_or(Errno::ENOENT)?;
        s.opens = s.opens.checked_add(1).expect("open count overflow");
        Ok(())
    }
    fn remove_open(&self, ino: u64) {
        let mut t = lock(&self.shared.table);
        if let Some(s) = t.map.get_mut(&ino) {
            s.opens = s.opens.saturating_sub(1);
        }
        t.maybe_drop(NodeId::from_ino(ino));
    }
    fn file(&self, fh: u64) -> Result<HandleLease<F::Handle>, Errno> {
        let record = lock(&self.handles)
            .files
            .get(&fh)
            .cloned()
            .ok_or(Errno::EBADF)?;
        record.acquire().ok_or(Errno::EBADF)
    }
    fn dir(&self, fh: u64) -> Option<HandleLease<F::DirHandle>> {
        let record = lock(&self.handles).dirs.get(&fh).cloned()?;
        record.acquire()
    }

    pub fn open(&self, ino: u64, flags: i32, caller: &Caller) -> Result<OpenReply, Errno> {
        let node = self.node(ino)?;
        let opened = self.fs.open(&node, flags, caller)?;
        self.add_open(ino)?;
        let fh = lock(&self.handles).add_file(opened.handle);
        Ok(OpenReply {
            fh,
            hints: opened.hints,
        })
    }

    /// Runs the reply continuation before releasing node and handle leases,
    /// allowing a borrowed `Cow` to be sent without copying.
    pub fn read<R>(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: usize,
        caller: &Caller,
        reply: impl FnOnce(Result<Cow<'_, [u8]>, Errno>) -> R,
    ) -> R {
        let node = match self.node(ino) {
            Ok(v) => v,
            Err(e) => return reply(Err(e)),
        };
        let handle = match self.file(fh) {
            Ok(v) => v,
            Err(e) => return reply(Err(e)),
        };
        reply(self.fs.read(&node, &handle, offset, size, caller))
    }

    pub fn write(
        &self,
        ino: u64,
        fh: u64,
        data: &[u8],
        offset: u64,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        let n = self.node(ino)?;
        let h = self.file(fh)?;
        self.fs.write(&n, &h, data, offset, caller)
    }
    pub fn flush(&self, ino: u64, fh: u64, caller: &Caller) -> Result<(), Errno> {
        let n = self.node(ino)?;
        let h = self.file(fh)?;
        self.fs.flush(&n, &h, caller)
    }
    pub fn fsync(&self, ino: u64, fh: u64, datasync: bool, caller: &Caller) -> Result<(), Errno> {
        let n = self.node(ino)?;
        let h = self.file(fh)?;
        self.fs.fsync(&n, &h, datasync, caller)
    }
    pub fn fallocate(
        &self,
        ino: u64,
        fh: u64,
        mode: i32,
        offset: u64,
        length: u64,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let n = self.node(ino)?;
        let h = self.file(fh)?;
        self.fs.fallocate(&n, &h, mode, offset, length, caller)
    }
    pub fn lseek(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        whence: i32,
        caller: &Caller,
    ) -> Result<u64, Errno> {
        let n = self.node(ino)?;
        let h = self.file(fh)?;
        self.fs.lseek(&n, &h, offset, whence, caller)
    }

    pub fn release(&self, ino: u64, fh: u64, caller: &Caller) -> Result<(), Errno> {
        let record = lock(&self.handles).files.remove(&fh).ok_or(Errno::EBADF)?;
        let node = self.node(ino)?;
        self.remove_open(ino);
        let handle = record.close_and_take();
        self.fs.release(&node, handle, caller)
    }

    pub fn opendir(&self, ino: u64, flags: i32, caller: &Caller) -> Result<OpenReply, Errno> {
        let node = self.node(ino)?;
        let opened = self.fs.opendir(&node, flags, caller)?;
        self.add_open(ino)?;
        let fh = lock(&self.handles).add_dir(opened.handle);
        Ok(OpenReply {
            fh,
            hints: opened.hints,
        })
    }

    pub fn readdir(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        sink: &mut dyn DirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let node = self.node(ino)?;
        let parent = lock(&self.shared.table)
            .map
            .get(&ino)
            .map(|s| s.parent)
            .ok_or(Errno::ENOENT)?;
        if let Some(h) = self.dir(fh) {
            self.fs.readdir(
                &node,
                NodeId::from_ino(ino),
                parent,
                &h,
                offset,
                sink,
                caller,
            )
        } else {
            let h = F::DirHandle::default();
            self.fs.readdir(
                &node,
                NodeId::from_ino(ino),
                parent,
                &h,
                offset,
                sink,
                caller,
            )
        }
    }

    pub fn fsyncdir(
        &self,
        ino: u64,
        fh: u64,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let node = self.node(ino)?;
        if let Some(h) = self.dir(fh) {
            self.fs.fsyncdir(&node, &h, datasync, caller)
        } else {
            self.fs
                .fsyncdir(&node, &F::DirHandle::default(), datasync, caller)
        }
    }

    pub fn releasedir(&self, ino: u64, fh: u64, caller: &Caller) -> Result<(), Errno> {
        let record = lock(&self.handles).dirs.remove(&fh);
        let node = self.node(ino)?;
        self.remove_open(ino);
        let handle = record.map(|r| r.close_and_take()).unwrap_or_default();
        self.fs.releasedir(&node, handle, caller)
    }

    pub fn lookup(&self, parent: u64, name: &str, caller: &Caller) -> Result<LookupReply, Errno> {
        match self
            .fs
            .lookup(&self.cx(), NodeId::from_ino(parent), name, caller)?
        {
            Some(id) => Ok(LookupReply::Found(self.entry_for(id, caller)?)),
            None => Ok(LookupReply::Negative),
        }
    }
    pub fn mknod(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        rdev: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = self.fs.mknod(
            &self.cx(),
            NodeId::from_ino(parent),
            name,
            mode,
            rdev,
            umask,
            caller,
        )?;
        self.entry_for(id, caller)
    }
    pub fn mkdir(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = self.fs.mkdir(
            &self.cx(),
            NodeId::from_ino(parent),
            name,
            mode,
            umask,
            caller,
        )?;
        self.entry_for(id, caller)
    }
    pub fn symlink(
        &self,
        parent: u64,
        name: &str,
        target: &str,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = self
            .fs
            .symlink(&self.cx(), NodeId::from_ino(parent), name, target, caller)?;
        self.entry_for(id, caller)
    }
    pub fn link(
        &self,
        ino: u64,
        newparent: u64,
        newname: &str,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = self.fs.link(
            &self.cx(),
            NodeId::from_ino(ino),
            NodeId::from_ino(newparent),
            newname,
            caller,
        )?;
        self.entry_for(id, caller)
    }
    pub fn unlink(&self, parent: u64, name: &str, caller: &Caller) -> Result<(), Errno> {
        self.fs
            .unlink(&self.cx(), NodeId::from_ino(parent), name, caller)
    }
    pub fn rmdir(&self, parent: u64, name: &str, caller: &Caller) -> Result<(), Errno> {
        self.fs
            .rmdir(&self.cx(), NodeId::from_ino(parent), name, caller)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        flags: u32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        self.fs.rename(
            &self.cx(),
            NodeId::from_ino(parent),
            name,
            NodeId::from_ino(newparent),
            newname,
            flags,
            caller,
        )
    }

    pub fn create(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        umask: u32,
        flags: i32,
        caller: &Caller,
    ) -> Result<(EntryReply, OpenReply), Errno> {
        let (id, opened) = self.fs.create(
            &self.cx(),
            NodeId::from_ino(parent),
            name,
            mode,
            umask,
            flags,
            caller,
        )?;
        let entry = self.entry_for(id, caller)?;
        self.add_open(id.ino())?;
        let fh = lock(&self.handles).add_file(opened.handle);
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
        &self,
        ino_in: u64,
        off_in: u64,
        ino_out: u64,
        off_out: u64,
        len: u64,
        flags: i32,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        self.fs.copy_file_range(
            &self.cx(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileKind, Opened};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Barrier, RwLock};
    use std::thread;

    enum Data {
        Dir(BTreeMap<String, NodeId>),
        File(Vec<u8>),
    }
    struct Node(RwLock<Data>);
    struct Mini;
    impl NodeFs for Mini {
        type Node = Node;
        type Handle = ();
        type DirHandle = ();
        fn root(&mut self) -> Node {
            Node(RwLock::new(Data::Dir(BTreeMap::new())))
        }
        fn getattr(&self, n: &Node, _: &Caller) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr {
                kind: match &*n.0.read().unwrap() {
                    Data::Dir(_) => FileKind::Directory,
                    Data::File(_) => FileKind::RegularFile,
                },
                nlink: 1,
                ..Default::default()
            })
        }
        fn create(
            &self,
            cx: &Cx<'_, Node>,
            parent: NodeId,
            name: &str,
            _: u32,
            _: u32,
            _: i32,
            _: &Caller,
        ) -> Result<(NodeId, Opened<()>), Errno> {
            let id = cx.insert(Node(RwLock::new(Data::File(Vec::new()))), parent);
            let p = cx.get(parent).ok_or(Errno::ENOENT)?;
            if let Data::Dir(e) = &mut *p.0.write().unwrap() {
                e.insert(name.into(), id);
            }
            Ok((id, Opened::new(())))
        }
        fn unlink(
            &self,
            cx: &Cx<'_, Node>,
            parent: NodeId,
            name: &str,
            _: &Caller,
        ) -> Result<(), Errno> {
            let p = cx.get(parent).ok_or(Errno::ENOENT)?;
            let id = match &mut *p.0.write().unwrap() {
                Data::Dir(e) => e.remove(name).ok_or(Errno::ENOENT)?,
                _ => return Err(Errno::ENOTDIR),
            };
            cx.remove_link(id);
            Ok(())
        }
        fn read<'a>(
            &'a self,
            n: &'a Node,
            _: &'a (),
            off: u64,
            size: usize,
            _: &Caller,
        ) -> Result<Cow<'a, [u8]>, Errno> {
            match &*n.0.read().unwrap() {
                Data::File(v) => Ok(Cow::Owned(
                    v.get(off as usize..).unwrap_or_default()
                        [..size.min(v.len().saturating_sub(off as usize))]
                        .to_vec(),
                )),
                _ => Err(Errno::EISDIR),
            }
        }
    }
    fn caller() -> Caller {
        Caller::default()
    }
    fn counts(rt: &Runtime<Mini>, ino: u64) -> Option<(u32, u64, u32, u32)> {
        lock(&rt.shared.table)
            .map
            .get(&ino)
            .map(|s| (s.links, s.lookups, s.opens, s.leases))
    }

    #[test]
    fn lifetime_and_generation() {
        let rt = Runtime::new(Mini);
        let (e, o) = rt.create(1, "f", 0, 0, 0, &caller()).unwrap();
        assert_eq!(counts(&rt, e.ino), Some((1, 1, 1, 0)));
        rt.unlink(1, "f", &caller()).unwrap();
        rt.forget(e.ino, 1);
        rt.release(e.ino, o.fh, &caller()).unwrap();
        assert_eq!(counts(&rt, e.ino), None);
        let (e2, _) = rt.create(1, "g", 0, 0, 0, &caller()).unwrap();
        assert_eq!(e2.ino, e.ino);
        assert_eq!(e2.generation, 1);
    }

    struct Overlap {
        barrier: Arc<Barrier>,
    }
    impl NodeFs for Overlap {
        type Node = ();
        type Handle = ();
        type DirHandle = ();
        fn root(&mut self) {}
        fn getattr(&self, _: &(), _: &Caller) -> Result<NodeAttr, Errno> {
            self.barrier.wait();
            Ok(NodeAttr::default())
        }
    }
    #[test]
    fn callbacks_really_overlap() {
        let rt = Arc::new(Runtime::new(Overlap {
            barrier: Arc::new(Barrier::new(2)),
        }));
        let a = Arc::clone(&rt);
        let b = Arc::clone(&rt);
        let t1 = thread::spawn(move || a.getattr(1, &caller()).unwrap());
        let t2 = thread::spawn(move || b.getattr(1, &caller()).unwrap());
        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn node_lease_defers_reuse() {
        let rt = Runtime::new(Mini);
        let (e, o) = rt.create(1, "f", 0, 0, 0, &caller()).unwrap();
        let lease = rt.node(e.ino).unwrap();
        rt.unlink(1, "f", &caller()).unwrap();
        rt.forget(e.ino, 1);
        rt.release(e.ino, o.fh, &caller()).unwrap();
        assert!(counts(&rt, e.ino).is_some());
        drop(lease);
        assert!(counts(&rt, e.ino).is_none());
    }

    struct BlockingHandle {
        entered: Arc<Barrier>,
        finish: Arc<Barrier>,
    }
    impl Default for BlockingHandle {
        fn default() -> Self {
            Self {
                entered: Arc::new(Barrier::new(1)),
                finish: Arc::new(Barrier::new(1)),
            }
        }
    }
    struct HandleOverlap {
        entered: Arc<Barrier>,
        finish: Arc<Barrier>,
        releases: Arc<AtomicUsize>,
    }
    impl NodeFs for HandleOverlap {
        type Node = ();
        type Handle = BlockingHandle;
        type DirHandle = ();
        fn root(&mut self) {}
        fn getattr(&self, _: &(), _: &Caller) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr::default())
        }
        fn open(&self, _: &(), _: i32, _: &Caller) -> Result<crate::Opened<BlockingHandle>, Errno> {
            Ok(crate::Opened::new(BlockingHandle {
                entered: Arc::clone(&self.entered),
                finish: Arc::clone(&self.finish),
            }))
        }
        fn read<'a>(
            &'a self,
            _: &'a (),
            h: &'a BlockingHandle,
            _: u64,
            _: usize,
            _: &Caller,
        ) -> Result<Cow<'a, [u8]>, Errno> {
            h.entered.wait();
            h.finish.wait();
            Ok(Cow::Borrowed(&[]))
        }
        fn release(&self, _: &(), _: BlockingHandle, _: &Caller) -> Result<(), Errno> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[test]
    fn release_waits_for_same_handle_callback_and_consumes_once() {
        let entered = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let releases = Arc::new(AtomicUsize::new(0));
        let rt = Arc::new(Runtime::new(HandleOverlap {
            entered: Arc::clone(&entered),
            finish: Arc::clone(&finish),
            releases: Arc::clone(&releases),
        }));
        let open = rt.open(1, 0, &caller()).unwrap();
        let reader = Arc::clone(&rt);
        let read_thread = thread::spawn(move || {
            reader
                .read(1, open.fh, 0, 1, &caller(), |r| r.map(|_| ()))
                .unwrap()
        });
        entered.wait();
        let closer = Arc::clone(&rt);
        let (tx, rx) = mpsc::channel();
        let close_thread = thread::spawn(move || {
            tx.send(closer.release(1, open.fh, &caller())).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        finish.wait();
        read_thread.join().unwrap();
        rx.recv().unwrap().unwrap();
        close_thread.join().unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert_eq!(rt.release(1, open.fh, &caller()).unwrap_err(), Errno::EBADF);
    }

    struct PanicOnce(AtomicBool);
    impl NodeFs for PanicOnce {
        type Node = ();
        type Handle = ();
        type DirHandle = ();
        fn root(&mut self) {}
        fn getattr(&self, _: &(), _: &Caller) -> Result<NodeAttr, Errno> {
            if self.0.swap(false, Ordering::SeqCst) {
                panic!("boom")
            }
            Ok(NodeAttr::default())
        }
    }
    #[test]
    fn panic_drops_node_lease_and_runtime_remains_usable() {
        let rt = Runtime::new(PanicOnce(AtomicBool::new(true)));
        assert!(std::panic::catch_unwind(
            std::panic::AssertUnwindSafe(|| rt.getattr(1, &caller()))
        )
        .is_err());
        assert!(rt.getattr(1, &caller()).is_ok());
        assert_eq!(lock(&rt.shared.table).map.get(&1).unwrap().leases, 0);
    }
}
