//! Safe-API port of libfuse's classic `hello_ll.c` example (see also the
//! raw-bindings version at `examples/hello_ll.rs` in the workspace root).
//!
//! A read-only filesystem with a single file `hello` containing
//! "Hello World!\n". Unlike the raw port, this file needs no manual memory
//! management, no C types, and no per-OS conditional compilation - it is
//! written entirely against the safe `fuse3::Filesystem` trait.
//!
//! Usage: `cargo run -p fuse3 --example hello_ll -- <mountpoint>`

use std::borrow::Cow;
use std::time::{Duration, SystemTime};

use fuse3::{
    AccessMode, DirBuffer, Entry, Errno, FileAttr, FileInfo, FileType, Filesystem, Inode,
    OpenReply, Request, Session, ROOT_INODE,
};

const HELLO_INODE: Inode = 2;
const HELLO_NAME: &str = "hello";
const HELLO_CONTENT: &[u8] = b"Hello World!\n";

struct HelloFs {
    mounted_at: SystemTime,
}

impl HelloFs {
    /// Returns the attributes for `ino`, or `None` if it does not exist.
    fn attr(&self, ino: Inode) -> Option<FileAttr> {
        let (kind, perm, nlink, size): (FileType, u16, u32, u64) = match ino {
            ROOT_INODE => (FileType::Directory, 0o755, 2, 0),
            HELLO_INODE => (FileType::RegularFile, 0o444, 1, HELLO_CONTENT.len() as u64),
            _ => return None,
        };
        Some(FileAttr {
            ino,
            size,
            kind,
            perm,
            nlink,
            atime: self.mounted_at,
            mtime: self.mounted_at,
            ctime: self.mounted_at,
            crtime: self.mounted_at,
            ..Default::default()
        })
    }
}

impl Filesystem for HelloFs {
    fn lookup(&mut self, _req: &Request, parent: Inode, name: &str) -> Result<Entry, Errno> {
        if parent != ROOT_INODE || name != HELLO_NAME {
            return Err(Errno::ENOENT);
        }
        let attr = self.attr(HELLO_INODE).ok_or(Errno::ENOENT)?;
        Ok(Entry {
            ino: HELLO_INODE,
            attr,
            attr_timeout: Duration::from_secs(1),
            entry_timeout: Duration::from_secs(1),
            ..Default::default()
        })
    }

    fn getattr(
        &mut self,
        _req: &Request,
        ino: Inode,
        _fh: Option<u64>,
    ) -> Result<(FileAttr, Duration), Errno> {
        self.attr(ino)
            .map(|attr| (attr, Duration::from_secs(1)))
            .ok_or(Errno::ENOENT)
    }

    fn open(&mut self, _req: &Request, ino: Inode, fi: &FileInfo) -> Result<OpenReply, Errno> {
        if ino != HELLO_INODE {
            return Err(Errno::EISDIR);
        }
        if fi.access_mode() != AccessMode::ReadOnly {
            return Err(Errno::EACCES);
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
        if ino != HELLO_INODE {
            return Err(Errno::EIO);
        }
        let offset = offset as usize;
        if offset >= HELLO_CONTENT.len() {
            return Ok(Cow::Borrowed(&[]));
        }
        let end = (offset + size).min(HELLO_CONTENT.len());
        Ok(Cow::Borrowed(&HELLO_CONTENT[offset..end]))
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: Inode,
        offset: u64,
        _fh: u64,
        buf: &mut DirBuffer,
    ) -> Result<(), Errno> {
        if ino != ROOT_INODE {
            return Err(Errno::ENOTDIR);
        }

        let entries: [(&str, Inode, FileType); 3] = [
            (".", ROOT_INODE, FileType::Directory),
            ("..", ROOT_INODE, FileType::Directory),
            (HELLO_NAME, HELLO_INODE, FileType::RegularFile),
        ];

        for (index, (name, ino, kind)) in entries.iter().enumerate().skip(offset as usize) {
            let next_offset = index as u64 + 1;
            if !buf.add(name, *ino, *kind, next_offset) {
                break;
            }
        }
        Ok(())
    }
}

fn main() {
    let mountpoint = match std::env::args().nth(1) {
        Some(mountpoint) => mountpoint,
        None => {
            eprintln!("usage: hello_ll <mountpoint>");
            std::process::exit(1);
        }
    };

    let fs = HelloFs {
        mounted_at: SystemTime::now(),
    };

    if let Err(e) = Session::mount_and_run(fs, &mountpoint, &[]) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
