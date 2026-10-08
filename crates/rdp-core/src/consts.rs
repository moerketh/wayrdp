//! Protocol/sync constants exposed by the freerdp-sys shim.
//!
//! FreeRDP/WinPR define these as `#define` macros or small C enums, neither of
//! which bindgen can see, so the shim exposes them as zero-argument fns. These
//! wrappers call them lazily and cache nothing; each is a couple of mov's.

use freerdp_sys::*;

#[inline]
pub fn cmdtype_set_surface_bits() -> UINT32 {
    unsafe { wayrdp_cmdtype_set_surface_bits() }
}
#[inline]
pub fn cmdtype_frame_marker() -> UINT32 {
    unsafe { wayrdp_cmdtype_frame_marker() }
}
#[inline]
pub fn frame_action_begin() -> UINT32 {
    unsafe { wayrdp_frame_action_begin() }
}
#[inline]
pub fn frame_action_end() -> UINT32 {
    unsafe { wayrdp_frame_action_end() }
}
#[inline]
pub fn wait_object_0() -> DWORD {
    unsafe { wayrdp_wait_object_0() }
}
#[inline]
pub fn wait_failed() -> DWORD {
    unsafe { wayrdp_wait_failed() }
}
#[inline]
pub fn infinite() -> DWORD {
    unsafe { wayrdp_infinite() }
}
#[inline]
pub fn pixel_format_bgrx32() -> UINT32 {
    unsafe { wayrdp_pixel_format_bgrx32() }
}
