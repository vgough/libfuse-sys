#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(clippy::useless_transmute)]
#![allow(clippy::cognitive_complexity)]
#![allow(clippy::missing_safety_doc)]

#[cfg(feature = "fuse_highlevel")]
use libc::*;

#[cfg(feature = "fuse_highlevel")]
pub mod fuse {
    use super::*;
    include!(concat!(env!("OUT_DIR"), "/fuse.rs"));

    /// Main function of FUSE
    ///
    /// Implemented as a macro in the original fuse header.
    pub unsafe fn fuse_main(
        argc: c_int,
        argv: *mut *mut c_char,
        op: *const fuse_operations,
        user_data: *mut c_void,
    ) -> c_int {
        let mut version = libfuse_version {
            major: FUSE_MAJOR_VERSION as _,
            minor: FUSE_MINOR_VERSION as _,
            hotfix: FUSE_HOTFIX_VERSION as _,
            ..Default::default()
        };
        #[cfg(target_os = "macos")]
        version.set_darwin_extensions_enabled(FUSE_DARWIN_ENABLE_EXTENSIONS as _);
        fuse_main_real_versioned(
            argc,
            argv,
            op,
            std::mem::size_of_val(&*op),
            &mut version,
            user_data,
        )
    }
}

#[cfg(feature = "fuse_lowlevel")]
#[allow(clashing_extern_declarations)]
pub mod fuse_lowlevel {
    include!(concat!(env!("OUT_DIR"), "/fuse_lowlevel.rs"));
}

#[cfg(feature = "cuse_lowlevel")]
pub mod cuse_lowlevel {
    include!(concat!(env!("OUT_DIR"), "/cuse_lowlevel.rs"));
}
