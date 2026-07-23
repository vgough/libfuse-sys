# libfuse-sys [![Latest Version]][crates.io]

[Latest Version]: https://img.shields.io/crates/v/libfuse-sys.svg
[crates.io]: https://crates.io/crates/libfuse-sys

**Raw rust bindings to libfuse**

---

## Using libfuse-sys

Add the dependencies to your Cargo.toml
```toml
[dependencies]
libfuse-sys = { version = "0.4", features = ["fuse_312"] }
libc = "0.2"
```
You can select a FUSE API version. Currently supported are
* `fuse_31`
* `fuse_35`
* `fuse_312` (requires libfuse 3.12 or later)

If no version is selected the crate defaults to version 35.

## Example

`examples/hello_ll_raw.rs` is a Rust port of libfuse's classic `hello_ll.c`: a read-only
filesystem exposing a single file, `hello`, containing "Hello World!\n". It uses the
lowlevel FUSE 3.x API.

```sh
mkdir /tmp/hello_mnt
cargo run --example hello_ll_raw -- /tmp/hello_mnt
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
`libfuse-sys`: implement the concurrent `NodeFs` trait with standard Rust types such as
`&OsStr` and `Result<T, Errno>`, then hand it to `Session` - no `unsafe`, no C types, no
`#[cfg(target_os = ...)]` required in your code.

```rust
use std::path::Path;
use fuse3::{Caller, Errno, NodeAttr, NodeFs, Session};

struct HelloFs;
struct Node;

impl NodeFs for HelloFs {
    type Node = Node;
    type Handle = ();
    type DirHandle = ();

    fn root(&mut self) -> Node { Node }
    fn getattr(
        &self,
        _node: &Node,
        _handle: Option<&()>,
        _caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Ok(NodeAttr::default())
    }
    // ... lookup, open, read, readdir
}

Session::mount_and_run(HelloFs, Path::new(&mountpoint), &[])?;
```

Sessions dispatch concurrently by default. Node and handle payloads are `Send + Sync`,
and implementations use interior synchronization for mutable state. A single-threaded
runtime mode is available through `SessionConfig`.

See `fuse3/README.md` for details and `fuse3/examples/hello_ll.rs` for the full example.

## License

This crate itself is published under the MIT license while libfuse is published under
LGPL2+. Take special care to ensure the terms of the LGPL2+ are honored when using this
crate.
