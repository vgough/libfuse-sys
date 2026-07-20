//! A safe, Rust-friendly wrapper over the raw low-level FUSE bindings in
//! `libfuse_sys::fuse_lowlevel`.
//!
//! Filesystem authors using this crate write zero `unsafe`, zero C types,
//! and zero `target_os` cfgs - all of that is centralized here.
//!
//! Implement [`Filesystem`] and drive it with [`Session`] (or the
//! [`Session::mount_and_run`] convenience function).

mod filesystem;
mod session;
mod types;

#[cfg(target_os = "macos")]
mod darwin;

pub use filesystem::Filesystem;
pub use session::{Error, MountOption, Session};
pub use types::{
    AccessMode, ConnInfo, DirBuffer, DirPlusBuffer, Entry, Errno, FileAttr, FileInfo, FileType,
    Inode, OpenReply, Request, SetAttrs, StatFs, TimeOrNow, XattrReply, ROOT_INODE,
};
