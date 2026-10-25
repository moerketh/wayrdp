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

/// Largest pointer every client accepts without the large-pointer
/// capability (MS-RDPBCGR TS_COLORPOINTERATTRIBUTE: width/height ≤ 96).
pub const MAX_POINTER_SIZE: u32 = 96;

/// Wire masks for a 32 bpp color pointer (MS-RDPBCGR §2.2.9.1.1.4.4):
/// both masks are scan-line ordered bottom-up, the XOR mask carries BGRA
/// with straight alpha, and the 1 bpp AND mask — rows padded to 2 bytes —
/// marks fully transparent pixels for clients that ignore alpha. Images
/// larger than [`MAX_POINTER_SIZE`] are cropped from the top-left, keeping
/// the hotspot inside the shape.
#[derive(Debug, PartialEq, Eq)]
pub struct PointerMasks {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub xor: Vec<u8>,
    pub and: Vec<u8>,
}

pub fn pointer_masks(img: &desk_backend::CursorImage) -> Option<PointerMasks> {
    let w = img.width.min(MAX_POINTER_SIZE);
    let h = img.height.min(MAX_POINTER_SIZE);
    let src_stride = img.width as usize * 4;
    if w == 0 || h == 0 || img.bgra.len() < src_stride * img.height as usize {
        return None;
    }
    let and_stride = (w as usize).div_ceil(16) * 2;
    let mut xor = Vec::with_capacity(w as usize * 4 * h as usize);
    let mut and = vec![0u8; and_stride * h as usize];
    for (out_row, src_row) in (0..h as usize).rev().enumerate() {
        let row = &img.bgra[src_row * src_stride..][..w as usize * 4];
        xor.extend_from_slice(row);
        for (x, px) in row.chunks_exact(4).enumerate() {
            if px[3] == 0 {
                and[out_row * and_stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    Some(PointerMasks {
        width: w as u16,
        height: h as u16,
        hot_x: img.hot_x.min(w - 1) as u16,
        hot_y: img.hot_y.min(h - 1) as u16,
        xor,
        and,
    })
}

/// Send the compositor's cursor image as the client's pointer (client-drawn
/// cursor mode). Needs `PointerNew` — the only PDU that carries a 32 bpp
/// shape with alpha; returns `false` when the client lacks it or the image
/// is unusable, so the caller can fall back to the transparent shape.
///
/// # Safety
/// Same preconditions as [`send_transparent_pointer`].
pub unsafe fn send_cursor_image(
    update: *mut rdpUpdate,
    context: *mut rdpContext,
    img: &desk_backend::CursorImage,
) -> bool {
    if update.is_null() || context.is_null() {
        return false;
    }
    let pointer = (*update).pointer;
    if pointer.is_null() {
        return false;
    }
    let (Some(newf), Some(m)) = ((*pointer).PointerNew, pointer_masks(img)) else {
        return false;
    };
    let mut np = POINTER_NEW_UPDATE {
        xorBpp: 32,
        colorPtrAttr: POINTER_COLOR_UPDATE {
            cacheIndex: 0,
            hotSpotX: m.hot_x as _,
            hotSpotY: m.hot_y as _,
            width: m.width as _,
            height: m.height as _,
            lengthAndMask: m.and.len() as _,
            lengthXorMask: m.xor.len() as _,
            xorMaskData: m.xor.as_ptr() as *mut BYTE,
            andMaskData: m.and.as_ptr() as *mut BYTE,
        },
    };
    let sent = newf(context, &mut np) != 0;
    if sent && std::env::var_os("WAYRDP_DEBUG_POINTER").is_some() {
        eprintln!(
            "[pointer] cursor {}x{} hot {},{} sent (xorBpp 32)",
            m.width, m.height, m.hot_x, m.hot_y
        );
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32, hot: (u32, u32), alpha: impl Fn(u32, u32) -> u8) -> desk_backend::CursorImage {
        let mut bgra = Vec::new();
        for y in 0..height {
            for x in 0..width {
                bgra.extend_from_slice(&[x as u8, y as u8, 0x10, alpha(x, y)]);
            }
        }
        desk_backend::CursorImage { width, height, hot_x: hot.0, hot_y: hot.1, bgra }
    }

    #[test]
    fn masks_are_bottom_up_with_word_aligned_and_rows() {
        // 3x2: top row opaque, bottom row transparent except x=2.
        let img = image(3, 2, (1, 1), |x, y| if y == 0 || x == 2 { 0xFF } else { 0 });
        let m = pointer_masks(&img).unwrap();
        assert_eq!((m.width, m.height, m.hot_x, m.hot_y), (3, 2, 1, 1));
        // First wire row is the image's BOTTOM row (y = 1).
        assert_eq!(&m.xor[0..4], &[0, 1, 0x10, 0]);
        assert_eq!(&m.xor[12..16], &[0, 0, 0x10, 0xFF]);
        assert_eq!(m.and.len(), 2 * 2, "3 px → one 16-bit word per row");
        assert_eq!(m.and[0], 0b1100_0000, "bottom row: x=0,1 transparent");
        assert_eq!(m.and[2], 0, "top row fully opaque");
    }

    #[test]
    fn oversized_cursor_is_cropped_and_hotspot_clamped() {
        let img = image(128, 100, (120, 50), |_, _| 0xFF);
        let m = pointer_masks(&img).unwrap();
        assert_eq!((m.width, m.height), (96, 96));
        assert_eq!((m.hot_x, m.hot_y), (95, 50));
        assert_eq!(m.xor.len(), 96 * 96 * 4);
        // Bottom wire row = source row 95, cropped to 96 px.
        assert_eq!(&m.xor[0..4], &[0, 95, 0x10, 0xFF]);
    }

    #[test]
    fn empty_or_short_images_are_rejected() {
        let mut img = image(4, 4, (0, 0), |_, _| 0xFF);
        img.bgra.truncate(10);
        assert!(pointer_masks(&img).is_none());
        assert!(pointer_masks(&image(0, 4, (0, 0), |_, _| 0)).is_none());
    }
}
