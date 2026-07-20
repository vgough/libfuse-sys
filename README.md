# libfuse-sys [![Latest Version]][crates.io] [![Build Status]][travis]

[Build Status]: https://travis-ci.org/Richard-W/libfuse-sys.svg?branch=master
[travis]: https://travis-ci.org/Richard-W/libfuse-sys
[Latest Version]: https://img.shields.io/crates/v/libfuse-sys.svg
[crates.io]: https://crates.io/crates/libfuse-sys

**Raw rust bindings to libfuse**

---

## Using libfuse-sys

Add the dependencies to your Cargo.toml
```toml
[dependencies]
libfuse-sys = { version = "*", features = ["fuse_35"] }
libc = "*"
```
You can select other API versions for fuse. Currently supported are
* `fuse_31`
* `fuse_35`

If no version is selected the crate defaults to version 35.

## Example

`examples/hello_ll.rs` is a Rust port of libfuse's classic `hello_ll.c`: a read-only
filesystem exposing a single file, `hello`, containing "Hello World!\n". It uses the
lowlevel FUSE 3.x API.

```sh
mkdir /tmp/hello_mnt
cargo run --example hello_ll -- /tmp/hello_mnt
```

In another terminal:
```sh
cat /tmp/hello_mnt/hello
```

Unmount it with `umount /tmp/hello_mnt` on macOS or `fusermount3 -u /tmp/hello_mnt` on
Linux.

## The `fuse3` crate: a safe wrapper

If you're writing a new filesystem, prefer the [`fuse3`](fuse3/) crate in this workspace
over the raw bindings above. It's a safe, Rust-friendly low-level FUSE API built on top of
`libfuse-sys`: implement the `Filesystem` trait with ordinary `&str`/`Result<T, Errno>`
methods and hand it to `Session` - no `unsafe`, no C types, no `#[cfg(target_os = ...)]`
required in your code.

```rust
impl Filesystem for HelloFs {
    fn lookup(&mut self, _req: &Request, parent: Inode, name: &str) -> Result<Entry, Errno> {
        if parent != ROOT_INODE || name != "hello" {
            return Err(Errno::ENOENT);
        }
        Ok(Entry { ino: 2, attr: self.hello_attr(), ..Default::default() })
    }
    // ... getattr, open, read, readdir
}

Session::mount_and_run(HelloFs::new(), &mountpoint, &[])?;
```

See `fuse3/README.md` for details and `fuse3/examples/hello_ll.rs` for the full example.

## License

This crate itself is published under the MIT license while libfuse is published under
LGPL2+. Take special care to ensure the terms of the LGPL2+ are honored when using this
crate.
