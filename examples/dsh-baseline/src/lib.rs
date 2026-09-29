//! Typed rutis bindings for published dsh Cordis plugins, generated from
//! their declarations during the normal Cargo build. Each module exposes
//! `Plugin`, `Config` and the native service proxy types.
#![cfg(all(unix, dsh_baseline))]

macro_rules! bindings {
    ($($module:ident),*) => {$(
        #[allow(clippy::all, dead_code)]
        pub mod $module {
            include!(concat!(env!("OUT_DIR"), "/", stringify!($module), ".rs"));
        }
    )*};
}

bindings!(invariants, credentials, fs, jobs, commands, workspace);
