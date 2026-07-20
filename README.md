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
* `fuse_11`
* `fuse_21`
* `fuse_22`
* `fuse_24`
* `fuse_25`
* `fuse_26`
* `fuse_29`
* `fuse_30`
* `fuse_31`
* `fuse_35`

If no version is selected the crate defaults to version 35.

## Example

`examples/hello_ll.rs` is a Rust port of libfuse's classic `hello_ll.c`: a read-only
filesystem exposing a single file, `hello`, containing "Hello World!\n". It uses the
lowlevel FUSE 3.x API, so it requires building without a FUSE 2.x version feature.

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

## License

This crate itself is published under the MIT license while libfuse is published under
LGPL2+. Take special care to ensure the terms of the LGPL2+ are honored when using this
crate.
