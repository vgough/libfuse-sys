# fuse3

**A safe, Rust-friendly low-level FUSE API, built on top of [`libfuse-sys`](..)**

`libfuse-sys` exposes only raw bindgen bindings to libfuse: writing a filesystem against
it means writing `unsafe` callbacks, using C types (`*const c_char`, `stat`,
`mem::zeroed()`), and sprinkling `#[cfg(target_os = "macos")]` around to handle libfuse's
Darwin-specific quirks. `fuse3` centralizes all of that here so filesystem authors write
none of it.

## What you get

- A `Filesystem` trait with methods like `lookup`, `getattr`, `read`, `write`, `readdir`,
  ... that take and return plain `std` types (`&str`, `Vec<u8>`, `SystemTime`,
  `Result<T, Errno>`). Every method has a sensible default, so you only implement the
  operations your filesystem actually supports.
- A `Session` type that owns mounting, the libfuse event loop, and unmounting.
- No `unsafe`, no C types (`stat`, `c_char`, raw pointers, ...), and no `target_os` cfgs in
  your code - all per-OS differences (timestamp field layout, Darwin's aliased
  `fuse_reply_*`/`fuse_add_direntry` symbols, mode_t width, ...) are handled internally.
- Filenames are `&str`/`String` - **UTF-8 only, by design**. An incoming name that isn't
  valid UTF-8 is rejected with `EILSEQ` before your filesystem ever sees it.
- Single-threaded: `Session::run` drives libfuse's `fuse_session_loop`, so `Filesystem`
  methods are never called concurrently and take `&mut self` with no `Send`/`Sync` bound.

## Minimal usage

```rust
use std::borrow::Cow;
use std::time::{Duration, SystemTime};
use fuse3::{
    DirBuffer, Entry, Errno, FileAttr, FileInfo, FileType, Filesystem, Inode, OpenReply,
    Request, Session, ROOT_INODE,
};

const HELLO_INODE: Inode = 2;
const HELLO_CONTENT: &[u8] = b"Hello World!\n";

struct HelloFs { mounted_at: SystemTime }

impl Filesystem for HelloFs {
    fn lookup(&mut self, _req: &Request, parent: Inode, name: &str) -> Result<Entry, Errno> {
        if parent != ROOT_INODE || name != "hello" {
            return Err(Errno::ENOENT);
        }
        let attr = FileAttr {
            ino: HELLO_INODE,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            size: HELLO_CONTENT.len() as u64,
            atime: self.mounted_at,
            mtime: self.mounted_at,
            ctime: self.mounted_at,
            ..Default::default()
        };
        Ok(Entry { ino: HELLO_INODE, attr, entry_timeout: Duration::from_secs(1), ..Default::default() })
    }

    fn open(&mut self, _req: &Request, ino: Inode, _fi: &FileInfo) -> Result<OpenReply, Errno> {
        if ino == HELLO_INODE { Ok(OpenReply::new(0)) } else { Err(Errno::EISDIR) }
    }

    fn read(&mut self, _req: &Request, _ino: Inode, size: usize, offset: u64, _fi: &FileInfo) -> Result<Cow<'_, [u8]>, Errno> {
        let offset = offset as usize;
        let end = (offset + size).min(HELLO_CONTENT.len());
        Ok(Cow::Borrowed(&HELLO_CONTENT[offset.min(end)..end]))
    }

    // ... getattr, readdir
}

fn main() {
    let mountpoint = std::env::args().nth(1).expect("usage: <mountpoint>");
    let fs = HelloFs { mounted_at: SystemTime::now() };
    Session::mount_and_run(fs, &mountpoint, &[]).expect("mount failed");
}
```

## Running the `hello_ll` example

`examples/hello_ll.rs` is the full safe-API port of libfuse's classic `hello_ll.c`: a
read-only filesystem exposing a single file, `hello`, containing `Hello World!\n`.

```sh
mkdir /tmp/hello_mnt
cargo run -p fuse3 --example hello_ll -- /tmp/hello_mnt
```

In another terminal:

```sh
cat /tmp/hello_mnt/hello
```

Unmount it with `umount /tmp/hello_mnt` on macOS or `fusermount3 -u /tmp/hello_mnt` on
Linux.

## License

This crate itself is published under the MIT license while libfuse is published under
LGPL2+. Take special care to ensure the terms of the LGPL2+ are honored when using this
crate.
