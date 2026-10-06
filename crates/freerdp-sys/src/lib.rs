//! Raw FreeRDP 3 bindings, generated at build time.
//!
//! This crate only exposes real symbols on Linux with `freerdp3-dev`
//! installed; elsewhere it compiles empty so the workspace stays checkable.

#[cfg(freerdp_present)]
mod inner {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(dead_code)]
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

#[cfg(freerdp_present)]
pub use inner::*;

#[cfg(not(freerdp_present))]
pub const FREERDP_MISSING: &str =
    "FreeRDP 3 dev libraries not found — install freerdp3-dev";
