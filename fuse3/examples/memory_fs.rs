//! Node-API port of a full read-write in-memory filesystem.
//!
//! Contrast with a raw low-level implementation: there is no inode table, no
//! inode allocator, no `get(&ino).ok_or(ENOENT)` on every call, no file-handle
//! table, and no `forget`/lifetime handling. The runtime owns identity and
//! lifetime; this filesystem stores per-node data, link counts, and directory
//! entries, and gets correct unlink-while-open behavior for free.
//!
//! Usage: `cargo run -p fuse3 --example memory_fs -- <mountpoint>`

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::time::SystemTime;

use fuse3::{
    Caller, Cx, DirSink, Errno, FileKind, NodeAttr, NodeFs, NodeId, Opened, Session, SetAttr,
    TimeOrNow,
};

#[derive(Clone, Copy)]
struct DirEntry {
    id: NodeId,
    kind: FileKind,
}

enum Content {
    Dir(BTreeMap<String, DirEntry>),
    File(Vec<u8>),
    Symlink(String),
    /// FIFO, socket, or device node; the kernel handles I/O directly.
    Special,
}

struct Node {
    kind: FileKind,
    content: Content,
    perm: u16,
    uid: u32,
    gid: u32,
    rdev: u32,
    nlink: u32,
    atime: SystemTime,
    mtime: SystemTime,
    ctime: SystemTime,
    crtime: SystemTime,
    xattrs: BTreeMap<String, Vec<u8>>,
}

impl Node {
    fn new(kind: FileKind, content: Content, perm: u16, uid: u32, gid: u32, rdev: u32) -> Self {
        let now = SystemTime::now();
        Node {
            kind,
            content,
            perm,
            uid,
            gid,
            rdev,
            nlink: if kind == FileKind::Directory { 2 } else { 1 },
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            xattrs: BTreeMap::new(),
        }
    }

    fn size(&self) -> u64 {
        match &self.content {
            Content::File(v) => v.len() as u64,
            Content::Symlink(s) => s.len() as u64,
            _ => 0,
        }
    }

    fn attr(&self) -> NodeAttr {
        NodeAttr {
            size: self.size(),
            kind: self.kind,
            perm: self.perm,
            uid: self.uid,
            gid: self.gid,
            rdev: self.rdev,
            nlink: self.nlink,
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            crtime: self.crtime,
            ..Default::default()
        }
    }

    fn touch(&mut self, now: SystemTime) {
        self.mtime = now;
        self.ctime = now;
    }
}

struct MemoryFs {
    uid: u32,
    gid: u32,
}

impl MemoryFs {
    fn new() -> Self {
        MemoryFs {
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// Validates that `parent` is a directory not already containing `name`.
    fn check_new_entry(cx: &Cx<'_, Node>, parent: NodeId, name: &str) -> Result<(), Errno> {
        match cx.get(parent) {
            Some(Node {
                content: Content::Dir(entries),
                ..
            }) => {
                if entries.contains_key(name) {
                    Err(Errno::EEXIST)
                } else {
                    Ok(())
                }
            }
            Some(_) => Err(Errno::ENOTDIR),
            None => Err(Errno::ENOENT),
        }
    }

    /// Records `id` under `name` in directory `parent` and bumps its times.
    fn link_into(
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        entry: DirEntry,
        now: SystemTime,
    ) {
        if let Some(node) = cx.get_mut(parent) {
            if let Content::Dir(entries) = &mut node.content {
                entries.insert(name.to_string(), entry);
            }
            if entry.kind == FileKind::Directory {
                node.nlink += 1;
            }
            node.touch(now);
        }
    }

    /// Inserts a fresh node, records it in `parent`, and returns its id.
    #[allow(clippy::too_many_arguments)]
    fn create_child(
        &self,
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        kind: FileKind,
        content: Content,
        perm: u16,
        rdev: u32,
    ) -> Result<NodeId, Errno> {
        Self::check_new_entry(cx, parent, name)?;
        let node = Node::new(kind, content, perm, self.uid, self.gid, rdev);
        let id = cx.insert(node, parent);
        Self::link_into(cx, parent, name, DirEntry { id, kind }, SystemTime::now());
        Ok(id)
    }
}

impl NodeFs for MemoryFs {
    type Node = Node;
    type Handle = ();
    type DirHandle = ();

    fn root(&mut self) -> Node {
        Node::new(
            FileKind::Directory,
            Content::Dir(BTreeMap::new()),
            0o755,
            self.uid,
            self.gid,
            0,
        )
    }

    fn getattr(&mut self, node: &Node, _c: &Caller) -> Result<NodeAttr, Errno> {
        Ok(node.attr())
    }

    fn setattr(&mut self, node: &mut Node, set: &SetAttr, _c: &Caller) -> Result<NodeAttr, Errno> {
        let now = SystemTime::now();
        let mut changed = false;

        if let Some(mode) = set.mode {
            node.perm = (mode & 0o7777) as u16;
            changed = true;
        }
        if let Some(uid) = set.uid {
            node.uid = uid;
            changed = true;
        }
        if let Some(gid) = set.gid {
            node.gid = gid;
            changed = true;
        }
        if let Some(size) = set.size {
            if let Content::File(content) = &mut node.content {
                content.resize(size as usize, 0);
            }
            changed = true;
        }
        if let Some(atime) = set.atime {
            node.atime = match atime {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => now,
            };
            changed = true;
        }
        if let Some(mtime) = set.mtime {
            node.mtime = match mtime {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => now,
            };
            changed = true;
        }
        if let Some(ctime) = set.ctime {
            node.ctime = ctime;
        } else if changed {
            node.ctime = now;
        }

        Ok(node.attr())
    }

    fn readlink(&mut self, node: &Node, _c: &Caller) -> Result<String, Errno> {
        match &node.content {
            Content::Symlink(target) => Ok(target.clone()),
            _ => Err(Errno::EINVAL),
        }
    }

    fn lookup(
        &mut self,
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        _c: &Caller,
    ) -> Result<Option<NodeId>, Errno> {
        match cx.get(parent) {
            Some(Node {
                content: Content::Dir(entries),
                ..
            }) => Ok(entries.get(name).map(|entry| entry.id)),
            Some(_) => Err(Errno::ENOTDIR),
            None => Err(Errno::ENOENT),
        }
    }

    fn mknod(
        &mut self,
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        mode: u32,
        rdev: u32,
        _umask: u32,
        _c: &Caller,
    ) -> Result<NodeId, Errno> {
        let (kind, content) = match mode & (libc::S_IFMT as u32) {
            x if x == libc::S_IFREG as u32 => (FileKind::RegularFile, Content::File(Vec::new())),
            x if x == libc::S_IFIFO as u32 => (FileKind::NamedPipe, Content::Special),
            x if x == libc::S_IFSOCK as u32 => (FileKind::Socket, Content::Special),
            x if x == libc::S_IFCHR as u32 => (FileKind::CharDevice, Content::Special),
            x if x == libc::S_IFBLK as u32 => (FileKind::BlockDevice, Content::Special),
            _ => return Err(Errno::EINVAL),
        };
        self.create_child(cx, parent, name, kind, content, (mode & 0o7777) as u16, rdev)
    }

    fn mkdir(
        &mut self,
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        mode: u32,
        _umask: u32,
        _c: &Caller,
    ) -> Result<NodeId, Errno> {
        self.create_child(
            cx,
            parent,
            name,
            FileKind::Directory,
            Content::Dir(BTreeMap::new()),
            (mode & 0o7777) as u16,
            0,
        )
    }

    fn symlink(
        &mut self,
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        target: &str,
        _c: &Caller,
    ) -> Result<NodeId, Errno> {
        self.create_child(
            cx,
            parent,
            name,
            FileKind::Symlink,
            Content::Symlink(target.to_string()),
            0o777,
            0,
        )
    }

    fn create(
        &mut self,
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        mode: u32,
        _umask: u32,
        _flags: i32,
        _c: &Caller,
    ) -> Result<(NodeId, Opened<()>), Errno> {
        let id = self.create_child(
            cx,
            parent,
            name,
            FileKind::RegularFile,
            Content::File(Vec::new()),
            (mode & 0o7777) as u16,
            0,
        )?;
        Ok((id, Opened::new(())))
    }

    fn unlink(
        &mut self,
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let id = match cx.get(parent) {
            Some(Node {
                content: Content::Dir(entries),
                ..
            }) => entries.get(name).ok_or(Errno::ENOENT)?.id,
            Some(_) => return Err(Errno::ENOTDIR),
            None => return Err(Errno::ENOENT),
        };
        if matches!(cx.get(id), Some(n) if n.kind == FileKind::Directory) {
            return Err(Errno::EISDIR);
        }
        let now = SystemTime::now();
        if let Some(Node {
            content: Content::Dir(entries),
            ..
        }) = cx.get_mut(parent)
        {
            entries.remove(name);
        }
        if let Some(node) = cx.get_mut(parent) {
            node.touch(now);
        }
        if let Some(node) = cx.get_mut(id) {
            node.nlink = node.nlink.saturating_sub(1);
            node.ctime = now;
        }
        cx.remove_link(id);
        Ok(())
    }

    fn rmdir(
        &mut self,
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let id = match cx.get(parent) {
            Some(Node {
                content: Content::Dir(entries),
                ..
            }) => entries.get(name).ok_or(Errno::ENOENT)?.id,
            Some(_) => return Err(Errno::ENOTDIR),
            None => return Err(Errno::ENOENT),
        };
        match cx.get(id) {
            Some(Node {
                content: Content::Dir(entries),
                ..
            }) => {
                if !entries.is_empty() {
                    return Err(Errno::ENOTEMPTY);
                }
            }
            Some(_) => return Err(Errno::ENOTDIR),
            None => return Err(Errno::ENOENT),
        }
        let now = SystemTime::now();
        if let Some(Node {
            content: Content::Dir(entries),
            ..
        }) = cx.get_mut(parent)
        {
            entries.remove(name);
        }
        if let Some(node) = cx.get_mut(parent) {
            node.nlink = node.nlink.saturating_sub(1);
            node.touch(now);
        }
        if let Some(node) = cx.get_mut(id) {
            node.nlink = 0;
            node.ctime = now;
        }
        cx.remove_link(id);
        Ok(())
    }

    fn rename(
        &mut self,
        cx: &mut Cx<'_, Node>,
        parent: NodeId,
        name: &str,
        newparent: NodeId,
        newname: &str,
        _flags: u32,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let target = match cx.get(parent) {
            Some(Node {
                content: Content::Dir(entries),
                ..
            }) => entries.get(name).ok_or(Errno::ENOENT)?.id,
            Some(_) => return Err(Errno::ENOTDIR),
            None => return Err(Errno::ENOENT),
        };
        let target_kind = cx.get(target).map(|node| node.kind).ok_or(Errno::ENOENT)?;
        let target_is_dir = target_kind == FileKind::Directory;

        // Inspect any entry being replaced at the destination.
        let existing = match cx.get(newparent) {
            Some(Node {
                content: Content::Dir(entries),
                ..
            }) => entries.get(newname).copied(),
            Some(_) => return Err(Errno::ENOTDIR),
            None => return Err(Errno::ENOENT),
        };
        if let Some(existing_entry) = existing {
            if existing_entry.id == target {
                return Ok(());
            }
            match cx.get(existing_entry.id).map(|n| &n.content) {
                Some(Content::Dir(entries)) => {
                    if !target_is_dir {
                        return Err(Errno::EISDIR);
                    }
                    if !entries.is_empty() {
                        return Err(Errno::ENOTEMPTY);
                    }
                }
                _ => {
                    if target_is_dir {
                        return Err(Errno::ENOTDIR);
                    }
                }
            }
        }

        let now = SystemTime::now();
        if let Some(Node {
            content: Content::Dir(entries),
            ..
        }) = cx.get_mut(parent)
        {
            entries.remove(name);
        }
        if let Some(node) = cx.get_mut(parent) {
            node.touch(now);
        }
        let replaced = if let Some(Node {
            content: Content::Dir(entries),
            ..
        }) = cx.get_mut(newparent)
        {
            entries.insert(
                newname.to_string(),
                DirEntry {
                    id: target,
                    kind: target_kind,
                },
            )
        } else {
            None
        };
        if let Some(node) = cx.get_mut(newparent) {
            node.touch(now);
        }
        if target_is_dir && parent != newparent {
            cx.reparent(target, newparent);
            if let Some(node) = cx.get_mut(parent) {
                node.nlink = node.nlink.saturating_sub(1);
            }
            if let Some(node) = cx.get_mut(newparent) {
                node.nlink += 1;
            }
        }
        if let Some(replaced_entry) = replaced {
            if replaced_entry.kind == FileKind::Directory {
                if let Some(node) = cx.get_mut(newparent) {
                    node.nlink = node.nlink.saturating_sub(1);
                }
            }
            if let Some(node) = cx.get_mut(replaced_entry.id) {
                if replaced_entry.kind == FileKind::Directory {
                    node.nlink = 0;
                } else {
                    node.nlink = node.nlink.saturating_sub(1);
                }
                node.ctime = now;
            }
            cx.remove_link(replaced_entry.id);
        }
        Ok(())
    }

    fn link(
        &mut self,
        cx: &mut Cx<'_, Node>,
        id: NodeId,
        newparent: NodeId,
        newname: &str,
        _c: &Caller,
    ) -> Result<NodeId, Errno> {
        if matches!(cx.get(id), Some(n) if n.kind == FileKind::Directory) {
            return Err(Errno::EPERM);
        }
        Self::check_new_entry(cx, newparent, newname)?;
        let now = SystemTime::now();
        let kind = cx.get(id).map(|node| node.kind).ok_or(Errno::ENOENT)?;
        Self::link_into(cx, newparent, newname, DirEntry { id, kind }, now);
        cx.add_link(id);
        if let Some(node) = cx.get_mut(id) {
            node.nlink += 1;
            node.ctime = now;
        }
        Ok(id)
    }

    fn open(&mut self, node: &mut Node, _flags: i32, _c: &Caller) -> Result<Opened<()>, Errno> {
        if node.kind == FileKind::Directory {
            return Err(Errno::EISDIR);
        }
        Ok(Opened::new(()))
    }

    fn read<'a>(
        &'a mut self,
        node: &'a mut Node,
        _h: &'a mut (),
        offset: u64,
        size: usize,
        _c: &Caller,
    ) -> Result<Cow<'a, [u8]>, Errno> {
        let Content::File(content) = &node.content else {
            return Err(Errno::EISDIR);
        };
        let offset = offset as usize;
        if offset >= content.len() {
            return Ok(Cow::Borrowed(&[]));
        }
        let end = (offset + size).min(content.len());
        Ok(Cow::Borrowed(&content[offset..end]))
    }

    fn write(
        &mut self,
        node: &mut Node,
        _h: &mut (),
        data: &[u8],
        offset: u64,
        _c: &Caller,
    ) -> Result<usize, Errno> {
        let now = SystemTime::now();
        let Content::File(content) = &mut node.content else {
            return Err(Errno::EISDIR);
        };
        let offset = offset as usize;
        let end = offset + data.len();
        if end > content.len() {
            content.resize(end, 0);
        }
        content[offset..end].copy_from_slice(data);
        node.touch(now);
        Ok(data.len())
    }

    fn readdir(
        &mut self,
        node: &Node,
        this: NodeId,
        parent: NodeId,
        _dh: &mut (),
        offset: u64,
        sink: &mut dyn DirSink,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let Content::Dir(entries) = &node.content else {
            return Err(Errno::ENOTDIR);
        };

        let mut cursor = offset;
        if cursor < 1 {
            if !sink.add(".", this, FileKind::Directory, 1) {
                return Ok(());
            }
            cursor = 1;
        }
        if cursor < 2 {
            if !sink.add("..", parent, FileKind::Directory, 2) {
                return Ok(());
            }
            cursor = 2;
        }

        let skip = (cursor - 2) as usize;
        for (i, (name, entry)) in entries.iter().enumerate().skip(skip) {
            let next_offset = (i + 3) as u64;
            if !sink.add(name, entry.id, entry.kind, next_offset) {
                break;
            }
        }
        Ok(())
    }

    // --- extended attributes (backed for real: on macOS an unsupported
    // xattr op makes the kernel spill to AppleDouble "._*" sidecar files). ---

    fn setxattr(
        &mut self,
        node: &mut Node,
        name: &str,
        value: &[u8],
        flags: i32,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let exists = node.xattrs.contains_key(name);
        if flags & libc::XATTR_CREATE != 0 && exists {
            return Err(Errno::EEXIST);
        }
        if flags & libc::XATTR_REPLACE != 0 && !exists {
            return Err(Errno::ENODATA);
        }
        node.xattrs.insert(name.to_string(), value.to_vec());
        node.ctime = SystemTime::now();
        Ok(())
    }

    fn getxattr(
        &mut self,
        node: &Node,
        name: &str,
        size: usize,
        _c: &Caller,
    ) -> Result<fuse3::XattrReply, Errno> {
        let value = node.xattrs.get(name).ok_or(Errno::ENODATA)?;
        if size == 0 {
            Ok(fuse3::XattrReply::Size(value.len()))
        } else {
            Ok(fuse3::XattrReply::Data(value.clone()))
        }
    }

    fn listxattr(
        &mut self,
        node: &Node,
        size: usize,
        _c: &Caller,
    ) -> Result<fuse3::XattrReply, Errno> {
        let mut names = Vec::new();
        for name in node.xattrs.keys() {
            names.extend_from_slice(name.as_bytes());
            names.push(0);
        }
        if size == 0 {
            Ok(fuse3::XattrReply::Size(names.len()))
        } else {
            Ok(fuse3::XattrReply::Data(names))
        }
    }

    fn removexattr(&mut self, node: &mut Node, name: &str, _c: &Caller) -> Result<(), Errno> {
        node.xattrs.remove(name).ok_or(Errno::ENODATA)?;
        node.ctime = SystemTime::now();
        Ok(())
    }
}

fn main() {
    let mountpoint = match std::env::args().nth(1) {
        Some(mountpoint) => mountpoint,
        None => {
            eprintln!("usage: memory_fs <mountpoint>");
            std::process::exit(1);
        }
    };

    if let Err(e) = Session::mount_and_run(MemoryFs::new(), &mountpoint, &[]) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use typed_fuse_core::Runtime;

    #[derive(Default)]
    struct Entries(Vec<(String, FileKind)>);

    impl DirSink for Entries {
        fn add(
            &mut self,
            name: &str,
            _id: NodeId,
            kind: FileKind,
            _next_offset: u64,
        ) -> bool {
            self.0.push((name.to_string(), kind));
            true
        }
    }

    #[test]
    fn directory_link_counts_follow_child_directories() {
        let mut rt = Runtime::new(MemoryFs::new());
        let caller = Caller::default();

        assert_eq!(rt.getattr(NodeId::ROOT.ino(), &caller).unwrap().nlink, 2);
        let child = rt
            .mkdir(NodeId::ROOT.ino(), "child", 0o755, 0, &caller)
            .unwrap();
        assert_eq!(child.attr.nlink, 2);
        assert_eq!(rt.getattr(NodeId::ROOT.ino(), &caller).unwrap().nlink, 3);

        rt.rmdir(NodeId::ROOT.ino(), "child", &caller).unwrap();
        assert_eq!(rt.getattr(NodeId::ROOT.ino(), &caller).unwrap().nlink, 2);
    }

    #[test]
    fn readdir_reports_each_entry_kind() {
        let mut rt = Runtime::new(MemoryFs::new());
        let caller = Caller::default();
        rt.mkdir(NodeId::ROOT.ino(), "dir", 0o755, 0, &caller)
            .unwrap();
        rt.symlink(NodeId::ROOT.ino(), "link", "target", &caller)
            .unwrap();
        rt.create(NodeId::ROOT.ino(), "file", 0o644, 0, 0, &caller)
            .unwrap();

        let open = rt.opendir(NodeId::ROOT.ino(), 0, &caller).unwrap();
        let mut entries = Entries::default();
        rt.readdir(
            NodeId::ROOT.ino(),
            open.fh,
            0,
            &mut entries,
            &caller,
        )
        .unwrap();

        assert!(entries.0.contains(&("dir".into(), FileKind::Directory)));
        assert!(entries.0.contains(&("link".into(), FileKind::Symlink)));
        assert!(entries
            .0
            .contains(&("file".into(), FileKind::RegularFile)));
    }
}
