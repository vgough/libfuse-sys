use std::borrow::Cow;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use fuse3::{
    DirBuffer, Entry, Errno, FileAttr, FileInfo, FileType, Filesystem, Inode, OpenReply, Request,
    Session, SetAttrs, TimeOrNow, XattrReply, ROOT_INODE,
};

#[derive(Clone)]
enum NodeData {
    Directory { entries: BTreeMap<String, Inode> },
    File { content: Vec<u8> },
    Symlink { target: String },
    /// FIFO, socket, or device node created via `mknod`. The kernel handles
    /// I/O on these directly once created, so no content is kept here.
    Special,
}

#[derive(Clone)]
struct Node {
    #[allow(dead_code)]
    ino: Inode,
    data: NodeData,
    attr: FileAttr,
    xattrs: BTreeMap<String, Vec<u8>>,
}

impl Node {
    fn new(ino: Inode, data: NodeData, attr: FileAttr) -> Self {
        Node {
            ino,
            data,
            attr,
            xattrs: BTreeMap::new(),
        }
    }
}

struct MemoryFS {
    nodes: BTreeMap<Inode, Node>,
    next_inode: Inode,
    owner_uid: u32,
    owner_gid: u32,
}

impl MemoryFS {
    fn new() -> Self {
        let mut fs = Self {
            nodes: BTreeMap::new(),
            next_inode: ROOT_INODE + 1,
            owner_uid: unsafe { libc::getuid() },
            owner_gid: unsafe { libc::getgid() },
        };
        let now = SystemTime::now();
        let attr = FileAttr {
            ino: ROOT_INODE,
            size: 0,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: fs.owner_uid,
            gid: fs.owner_gid,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            ..Default::default()
        };
        fs.nodes.insert(
            ROOT_INODE,
            Node::new(
                ROOT_INODE,
                NodeData::Directory {
                    entries: BTreeMap::new(),
                },
                attr,
            ),
        );
        fs
    }

    fn alloc_inode(&mut self) -> Inode {
        let ino = self.next_inode;
        self.next_inode += 1;
        ino
    }
}

impl Filesystem for MemoryFS {
    fn lookup(&mut self, _req: &Request, parent: Inode, name: &str) -> Result<Entry, Errno> {
        let parent_node = self.nodes.get(&parent).ok_or(Errno::ENOENT)?;
        if let NodeData::Directory { entries } = &parent_node.data {
            if let Some(&ino) = entries.get(name) {
                let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
                return Ok(Entry {
                    ino,
                    attr: node.attr,
                    attr_timeout: Duration::from_secs(1),
                    entry_timeout: Duration::from_secs(1),
                    ..Default::default()
                });
            } else {
                return Err(Errno::ENOENT);
            }
        }
        Err(Errno::ENOTDIR)
    }

    fn getattr(
        &mut self,
        _req: &Request,
        ino: Inode,
        _fh: Option<u64>,
    ) -> Result<(FileAttr, Duration), Errno> {
        let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        Ok((node.attr, Duration::from_secs(1)))
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: Inode,
        attrs: &SetAttrs,
        _fh: Option<u64>,
    ) -> Result<(FileAttr, Duration), Errno> {
        let node = self.nodes.get_mut(&ino).ok_or(Errno::ENOENT)?;
        let now = SystemTime::now();
        let mut changed = false;

        if let Some(mode) = attrs.mode {
            node.attr.perm = (mode & 0o7777) as u16;
            changed = true;
        }
        if let Some(uid) = attrs.uid {
            node.attr.uid = uid;
            changed = true;
        }
        if let Some(gid) = attrs.gid {
            node.attr.gid = gid;
            changed = true;
        }
        if let Some(size) = attrs.size {
            node.attr.size = size;
            if let NodeData::File { content } = &mut node.data {
                content.resize(size as usize, 0);
            }
            changed = true;
        }
        if let Some(atime) = attrs.atime {
            node.attr.atime = match atime {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => now,
            };
            changed = true;
        }
        if let Some(mtime) = attrs.mtime {
            node.attr.mtime = match mtime {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => now,
            };
            changed = true;
        }

        // The kernel only supplies an explicit ctime on macOS's setattr_x
        // path; on ordinary chmod/chown/truncate we must bump it ourselves,
        // since ctime is expected to change whenever inode metadata does.
        if let Some(ctime) = attrs.ctime {
            node.attr.ctime = ctime;
        } else if changed {
            node.attr.ctime = now;
        }

        Ok((node.attr, Duration::from_secs(1)))
    }

    fn readlink(&mut self, _req: &Request, ino: Inode) -> Result<String, Errno> {
        let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        if let NodeData::Symlink { target } = &node.data {
            Ok(target.clone())
        } else {
            Err(Errno::EINVAL)
        }
    }

    // Extended attributes must be backed for real, rather than left at the
    // trait's ENOSYS default: on macOS, a filesystem that doesn't support
    // xattrs makes the kernel fall back to shadow AppleDouble "._name"
    // files to hold them, and those sidecar files then show up as regular
    // directory entries (e.g. matching a `*.idx` glob in a git pack
    // directory and getting parsed as a corrupt pack index).
    fn setxattr(
        &mut self,
        _req: &Request,
        ino: Inode,
        name: &str,
        value: &[u8],
        flags: i32,
    ) -> Result<(), Errno> {
        let node = self.nodes.get_mut(&ino).ok_or(Errno::ENOENT)?;
        let exists = node.xattrs.contains_key(name);
        if flags & libc::XATTR_CREATE != 0 && exists {
            return Err(Errno::EEXIST);
        }
        if flags & libc::XATTR_REPLACE != 0 && !exists {
            return Err(Errno::ENODATA);
        }
        node.xattrs.insert(name.to_string(), value.to_vec());
        node.attr.ctime = SystemTime::now();
        Ok(())
    }

    fn getxattr(
        &mut self,
        _req: &Request,
        ino: Inode,
        name: &str,
        size: usize,
    ) -> Result<XattrReply, Errno> {
        let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        let value = node.xattrs.get(name).ok_or(Errno::ENODATA)?;
        if size == 0 {
            Ok(XattrReply::Size(value.len()))
        } else {
            Ok(XattrReply::Data(value.clone()))
        }
    }

    fn listxattr(&mut self, _req: &Request, ino: Inode, size: usize) -> Result<XattrReply, Errno> {
        let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        let mut names = Vec::new();
        for name in node.xattrs.keys() {
            names.extend_from_slice(name.as_bytes());
            names.push(0);
        }
        if size == 0 {
            Ok(XattrReply::Size(names.len()))
        } else {
            Ok(XattrReply::Data(names))
        }
    }

    fn removexattr(&mut self, _req: &Request, ino: Inode, name: &str) -> Result<(), Errno> {
        let node = self.nodes.get_mut(&ino).ok_or(Errno::ENOENT)?;
        node.xattrs.remove(name).ok_or(Errno::ENODATA)?;
        node.attr.ctime = SystemTime::now();
        Ok(())
    }

    fn mknod(
        &mut self,
        _req: &Request,
        parent: Inode,
        name: &str,
        mode: u32,
        rdev: u32,
    ) -> Result<Entry, Errno> {
        let parent_node = self.nodes.get_mut(&parent).ok_or(Errno::ENOENT)?;
        if let NodeData::Directory { entries } = &mut parent_node.data {
            if entries.contains_key(name) {
                return Err(Errno::EEXIST);
            }
        } else {
            return Err(Errno::ENOTDIR);
        }

        let (kind, data) = match mode & (libc::S_IFMT as u32) {
            x if x == libc::S_IFREG as u32 => (
                FileType::RegularFile,
                NodeData::File { content: Vec::new() },
            ),
            x if x == libc::S_IFIFO as u32 => (FileType::NamedPipe, NodeData::Special),
            x if x == libc::S_IFSOCK as u32 => (FileType::Socket, NodeData::Special),
            x if x == libc::S_IFCHR as u32 => (FileType::CharDevice, NodeData::Special),
            x if x == libc::S_IFBLK as u32 => (FileType::BlockDevice, NodeData::Special),
            _ => return Err(Errno::EINVAL),
        };

        let ino = self.alloc_inode();
        let now = SystemTime::now();
        let attr = FileAttr {
            ino,
            size: 0,
            kind,
            perm: (mode & 0o7777) as u16,
            nlink: 1,
            uid: self.owner_uid,
            gid: self.owner_gid,
            rdev,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            ..Default::default()
        };

        if let NodeData::Directory { entries } = &mut self.nodes.get_mut(&parent).unwrap().data {
            entries.insert(name.to_string(), ino);
        }

        let p_node = self.nodes.get_mut(&parent).unwrap();
        p_node.attr.mtime = now;
        p_node.attr.ctime = now;

        self.nodes.insert(ino, Node::new(ino, data, attr));

        Ok(Entry {
            ino,
            attr,
            attr_timeout: Duration::from_secs(1),
            entry_timeout: Duration::from_secs(1),
            ..Default::default()
        })
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: Inode,
        name: &str,
        mode: u32,
    ) -> Result<Entry, Errno> {
        let parent_node = self.nodes.get_mut(&parent).ok_or(Errno::ENOENT)?;
        if let NodeData::Directory { entries } = &mut parent_node.data {
            if entries.contains_key(name) {
                return Err(Errno::EEXIST);
            }
        } else {
            return Err(Errno::ENOTDIR);
        }

        let ino = self.alloc_inode();
        let now = SystemTime::now();
        let attr = FileAttr {
            ino,
            size: 0,
            kind: FileType::Directory,
            perm: (mode & 0o7777) as u16,
            nlink: 2,
            uid: self.owner_uid,
            gid: self.owner_gid,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            ..Default::default()
        };

        if let NodeData::Directory { entries } = &mut self.nodes.get_mut(&parent).unwrap().data {
            entries.insert(name.to_string(), ino);
        }

        let p_node = self.nodes.get_mut(&parent).unwrap();
        p_node.attr.nlink += 1;
        p_node.attr.mtime = now;
        p_node.attr.ctime = now;

        self.nodes.insert(
            ino,
            Node::new(
                ino,
                NodeData::Directory {
                    entries: BTreeMap::new(),
                },
                attr,
            ),
        );

        Ok(Entry {
            ino,
            attr,
            attr_timeout: Duration::from_secs(1),
            entry_timeout: Duration::from_secs(1),
            ..Default::default()
        })
    }

    fn unlink(&mut self, _req: &Request, parent: Inode, name: &str) -> Result<(), Errno> {
        let parent_node = self.nodes.get(&parent).ok_or(Errno::ENOENT)?;
        let target_ino = if let NodeData::Directory { entries } = &parent_node.data {
            *entries.get(name).ok_or(Errno::ENOENT)?
        } else {
            return Err(Errno::ENOTDIR);
        };

        let target_node = self.nodes.get(&target_ino).ok_or(Errno::ENOENT)?;
        if let NodeData::Directory { .. } = target_node.data {
            return Err(Errno::EISDIR);
        }

        if let NodeData::Directory { entries } = &mut self.nodes.get_mut(&parent).unwrap().data {
            entries.remove(name);
        }

        let now = SystemTime::now();
        let p_node = self.nodes.get_mut(&parent).unwrap();
        p_node.attr.mtime = now;
        p_node.attr.ctime = now;

        let t_node = self.nodes.get_mut(&target_ino).unwrap();
        t_node.attr.nlink -= 1;
        if t_node.attr.nlink == 0 {
            self.nodes.remove(&target_ino);
        }

        Ok(())
    }

    fn rmdir(&mut self, _req: &Request, parent: Inode, name: &str) -> Result<(), Errno> {
        let parent_node = self.nodes.get(&parent).ok_or(Errno::ENOENT)?;
        let target_ino = if let NodeData::Directory { entries } = &parent_node.data {
            *entries.get(name).ok_or(Errno::ENOENT)?
        } else {
            return Err(Errno::ENOTDIR);
        };

        let target_node = self.nodes.get(&target_ino).ok_or(Errno::ENOENT)?;
        if let NodeData::Directory { entries } = &target_node.data {
            if !entries.is_empty() {
                return Err(Errno::ENOTEMPTY);
            }
        } else {
            return Err(Errno::ENOTDIR);
        }

        if let NodeData::Directory { entries } = &mut self.nodes.get_mut(&parent).unwrap().data {
            entries.remove(name);
        }

        let now = SystemTime::now();
        let p_node = self.nodes.get_mut(&parent).unwrap();
        p_node.attr.nlink -= 1;
        p_node.attr.mtime = now;
        p_node.attr.ctime = now;

        self.nodes.remove(&target_ino);
        Ok(())
    }

    fn symlink(
        &mut self,
        _req: &Request,
        parent: Inode,
        name: &str,
        link: &str,
    ) -> Result<Entry, Errno> {
        let parent_node = self.nodes.get_mut(&parent).ok_or(Errno::ENOENT)?;
        if let NodeData::Directory { entries } = &mut parent_node.data {
            if entries.contains_key(name) {
                return Err(Errno::EEXIST);
            }
        } else {
            return Err(Errno::ENOTDIR);
        }

        let ino = self.alloc_inode();
        let now = SystemTime::now();
        let attr = FileAttr {
            ino,
            size: link.len() as u64,
            kind: FileType::Symlink,
            perm: 0o777,
            nlink: 1,
            uid: self.owner_uid,
            gid: self.owner_gid,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            ..Default::default()
        };

        if let NodeData::Directory { entries } = &mut self.nodes.get_mut(&parent).unwrap().data {
            entries.insert(name.to_string(), ino);
        }

        let p_node = self.nodes.get_mut(&parent).unwrap();
        p_node.attr.mtime = now;
        p_node.attr.ctime = now;

        self.nodes.insert(
            ino,
            Node::new(
                ino,
                NodeData::Symlink {
                    target: link.to_string(),
                },
                attr,
            ),
        );

        Ok(Entry {
            ino,
            attr,
            attr_timeout: Duration::from_secs(1),
            entry_timeout: Duration::from_secs(1),
            ..Default::default()
        })
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: Inode,
        name: &str,
        newparent: Inode,
        newname: &str,
        _flags: u32,
    ) -> Result<(), Errno> {
        let p_node = self.nodes.get(&parent).ok_or(Errno::ENOENT)?;
        let target_ino = if let NodeData::Directory { entries } = &p_node.data {
            *entries.get(name).ok_or(Errno::ENOENT)?
        } else {
            return Err(Errno::ENOTDIR);
        };
        let target_is_dir = matches!(
            self.nodes.get(&target_ino).ok_or(Errno::ENOENT)?.data,
            NodeData::Directory { .. }
        );

        let new_p_node = self.nodes.get(&newparent).ok_or(Errno::ENOENT)?;
        let mut existing_is_dir = false;
        if let NodeData::Directory { entries } = &new_p_node.data {
            if let Some(&existing_ino) = entries.get(newname) {
                if existing_ino == target_ino {
                    // Renaming an entry onto itself: nothing to do.
                    return Ok(());
                }
                let existing_node = self.nodes.get(&existing_ino).unwrap();
                match &existing_node.data {
                    NodeData::Directory { entries: e2 } => {
                        if !target_is_dir {
                            return Err(Errno::EISDIR);
                        }
                        if !e2.is_empty() {
                            return Err(Errno::ENOTEMPTY);
                        }
                        existing_is_dir = true;
                    }
                    _ => {
                        if target_is_dir {
                            return Err(Errno::ENOTDIR);
                        }
                    }
                }
            }
        } else {
            return Err(Errno::ENOTDIR);
        }

        if let NodeData::Directory { entries } = &mut self.nodes.get_mut(&parent).unwrap().data {
            entries.remove(name);
        }

        let mut removed_ino = None;
        if let NodeData::Directory { entries } = &mut self.nodes.get_mut(&newparent).unwrap().data {
            removed_ino = entries.insert(newname.to_string(), target_ino);
        }

        let now = SystemTime::now();
        let p1 = self.nodes.get_mut(&parent).unwrap();
        p1.attr.mtime = now;
        p1.attr.ctime = now;

        if parent != newparent {
            let p2 = self.nodes.get_mut(&newparent).unwrap();
            p2.attr.mtime = now;
            p2.attr.ctime = now;

            if target_is_dir {
                // The moved directory's ".." now points at newparent
                // instead of parent, so the nlink contribution moves too.
                self.nodes.get_mut(&parent).unwrap().attr.nlink -= 1;
                self.nodes.get_mut(&newparent).unwrap().attr.nlink += 1;
            }
        }

        if let Some(r_ino) = removed_ino {
            if existing_is_dir {
                // The replaced directory's ".." no longer contributes to
                // newparent's nlink.
                self.nodes.get_mut(&newparent).unwrap().attr.nlink -= 1;
            }
            let r_node = self.nodes.get_mut(&r_ino).unwrap();
            r_node.attr.nlink -= 1;
            if r_node.attr.nlink == 0 {
                self.nodes.remove(&r_ino);
            }
        }

        Ok(())
    }

    fn open(&mut self, _req: &Request, ino: Inode, _fi: &FileInfo) -> Result<OpenReply, Errno> {
        let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        if let NodeData::Directory { .. } = &node.data {
            return Err(Errno::EISDIR);
        }
        Ok(OpenReply::new(0))
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: Inode,
        size: usize,
        offset: u64,
        _fi: &FileInfo,
    ) -> Result<Cow<'_, [u8]>, Errno> {
        let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        if let NodeData::File { content } = &node.data {
            let offset = offset as usize;
            if offset >= content.len() {
                return Ok(Cow::Borrowed(&[]));
            }
            let end = (offset + size).min(content.len());
            Ok(Cow::Borrowed(&content[offset..end]))
        } else {
            Err(Errno::EISDIR)
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: Inode,
        data: &[u8],
        offset: u64,
        _fi: &FileInfo,
    ) -> Result<usize, Errno> {
        let node = self.nodes.get_mut(&ino).ok_or(Errno::ENOENT)?;
        if let NodeData::File { content } = &mut node.data {
            let offset = offset as usize;
            let end = offset + data.len();
            if end > content.len() {
                content.resize(end, 0);
            }
            content[offset..end].copy_from_slice(data);
            node.attr.size = content.len() as u64;
            let now = SystemTime::now();
            node.attr.mtime = now;
            node.attr.ctime = now;
            Ok(data.len())
        } else {
            Err(Errno::EISDIR)
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: Inode,
        offset: u64,
        _fh: u64,
        buf: &mut DirBuffer,
    ) -> Result<(), Errno> {
        let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        if let NodeData::Directory { entries } = &node.data {
            let mut i = offset;

            if i == 0 {
                if !buf.add(".", ino, FileType::Directory, 1) {
                    return Ok(());
                }
                i += 1;
            }
            if i == 1 {
                if !buf.add("..", ROOT_INODE, FileType::Directory, 2) {
                    return Ok(());
                }
                i += 1;
            }

            let skip = (i - 2) as usize;
            for (idx, (name, &child_ino)) in entries.iter().enumerate().skip(skip) {
                let child_node = self.nodes.get(&child_ino).unwrap();
                let next_offset = (idx + 3) as u64;
                if !buf.add(name, child_ino, child_node.attr.kind, next_offset) {
                    break;
                }
            }
            Ok(())
        } else {
            Err(Errno::ENOTDIR)
        }
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: Inode,
        name: &str,
        mode: u32,
        _fi: &FileInfo,
    ) -> Result<(Entry, OpenReply), Errno> {
        let parent_node = self.nodes.get_mut(&parent).ok_or(Errno::ENOENT)?;
        if let NodeData::Directory { entries } = &mut parent_node.data {
            if entries.contains_key(name) {
                return Err(Errno::EEXIST);
            }
        } else {
            return Err(Errno::ENOTDIR);
        }

        let ino = self.alloc_inode();
        let now = SystemTime::now();
        let attr = FileAttr {
            ino,
            size: 0,
            kind: FileType::RegularFile,
            perm: (mode & 0o7777) as u16,
            nlink: 1,
            uid: self.owner_uid,
            gid: self.owner_gid,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            ..Default::default()
        };

        if let NodeData::Directory { entries } = &mut self.nodes.get_mut(&parent).unwrap().data {
            entries.insert(name.to_string(), ino);
        }

        let p_node = self.nodes.get_mut(&parent).unwrap();
        p_node.attr.mtime = now;
        p_node.attr.ctime = now;

        self.nodes.insert(
            ino,
            Node::new(
                ino,
                NodeData::File {
                    content: Vec::new(),
                },
                attr,
            ),
        );

        let entry = Entry {
            ino,
            attr,
            attr_timeout: Duration::from_secs(1),
            entry_timeout: Duration::from_secs(1),
            ..Default::default()
        };
        Ok((entry, OpenReply::new(0)))
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

    let fs = MemoryFS::new();

    if let Err(e) = Session::mount_and_run(fs, &mountpoint, &[]) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
