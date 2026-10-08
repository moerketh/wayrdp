//! Transparent color-pointer PDU (MS-RDP-BCGR §2.2.9.1.1.4) for clients
//! that ignore SYSPTR_NULL and fall back to their local arrow — Hyper-V
//! vmconnect measures as that client here. All AND bits set and all XOR
//! bytes zero renders a shape with nothing visible, so the client swaps
//! its own arrow out instead of drawing a second cursor next to the
//! compositor-painted one. Re-sent because client pointer state can reset
//! on EGFX reset / resize.
//!
//! Carry-over: hyperv-rdp-extras `src/cursor.rs` — spec-derived shape
//! fields only, encoded against FreeRDP 3's `POINTER_COLOR_UPDATE`.

use freerdp_sys::*;

/// Send a `PTR_POS_UPDATE` (MS-RDP-BCGR §2.2.9.1.1.4.3): the client re-
/// positions the cursor it last drew (e.g. our transparent shape from
/// `send_transparent_pointer`) without changing its pixels. Combined with
/// the transparent shape, this yields the "Metadata" cursor mode — the
/// cursor the user sees is drawn by the client at position-update latency,
/// while the captured video keeps the Hyprland-rendered cursor hidden
/// behind its all-zero-pixel shape. Position coordinates are layout pixels.
///
/// # Safety
/// Same preconditions as `send_transparent_pointer`: `update` must point at
/// a valid live `rdpUpdate` whose `pointer` has `PointerPosition` populated
/// (all clients we ship to support it), and `context` must be the peer's own
/// `rdpContext`. Called from the peer thread's mainloop.
pub unsafe fn send_pointer_position(
    update: *mut rdpUpdate,
    context: *mut rdpContext,
    x: u32,
    y: u32,
) -> bool {
    if update.is_null() || context.is_null() {
        return false;
    }
    let pointer = (*update).pointer;
    if pointer.is_null() {
        return false;
    }
    let Some(pos_fn) = (*pointer).PointerPosition else {
        return false;
    };
    let mut pd = POINTER_POSITION_UPDATE { xPos: x, yPos: y };
    pos_fn(context, &mut pd) != 0
}

/// Build and send the transparent 32×32 color-pointer. Prefers
/// `PointerNew` when present (xorBpp 24 matches our XOR mask encoding),
/// otherwise `PointerColor`. Returns `true` when a callback accepted it.
///
/// # Safety
/// `context` must be the peer's own, valid `rdpContext` — the
/// `rdpPointerUpdate.context` back-link is NOT populated on the server
/// core path (observed NULL at activate; FreeRDP's shadow server likewise
/// passes its own client context, never `update->pointer->context`).
/// `update` must point at a valid, live `rdpUpdate` (and its `pointer` at
/// a valid `rdpPointerUpdate`).
pub unsafe fn send_transparent_pointer(update: *mut rdpUpdate, context: *mut rdpContext) -> bool {
    if update.is_null() || context.is_null() {
        return false;
    }
    let pointer = (*update).pointer;
    if pointer.is_null() {
        return false;
    }
    let and = [0xFFu8; 32 * 4]; // 1 bit/px, 32 px/row = 4 bytes/row
    let xor = [0x00u8; 32 * 96]; // 24 bpp = 3 bytes/px × 32 = 96 bytes/row
    let mut pd = POINTER_COLOR_UPDATE {
        cacheIndex: 0,
        hotSpotX: 0,
        hotSpotY: 0,
        width: 32,
        height: 32,
        lengthAndMask: and.len() as UINT16,
        lengthXorMask: xor.len() as UINT16,
        xorMaskData: xor.as_ptr() as *mut BYTE,
        andMaskData: and.as_ptr() as *mut BYTE,
    };
    let sent = if let Some(newf) = (*pointer).PointerNew {
        let mut np = POINTER_NEW_UPDATE {
            xorBpp: 24,
            colorPtrAttr: pd,
        };
        newf(context, &mut np) != 0
    } else if let Some(col) = (*pointer).PointerColor {
        col(context, &mut pd) != 0
    } else {
        false
    };
    if sent && std::env::var_os("WAYRDP_DEBUG_POINTER").is_some() {
        eprintln!("[pointer] transparent 32x32 sent (xorBpp 24)");
    }
    sent
}
