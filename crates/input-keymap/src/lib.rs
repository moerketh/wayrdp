//! Input helpers shared by wayrdp's desktop backends (moved unchanged out of
//! backend-wlroots so backend-kde can use them without depending on it):
//! RDP scancode -> evdev keycode (WinPR tables plus the holes patch),
//! evdev modifier bits, and the CLOCK_MONOTONIC u32 stamp shared with the
//! capture plugin and rdp-core's input stamps.

/// CLOCK_MONOTONIC milliseconds truncated to u32 (wraps every ~49 days —
/// fine, all deltas use wrapping_sub). Shared clock domain with the capture
/// plugin's header stamp, so cross-component queue latencies are directly
/// comparable. 0 is reserved as "unstamped"; a real stamp of exactly 0 is
/// astronomically unlikely (boot-second boundary) and at worst drops one
/// sample.
pub fn now_mono_ms_u32() -> u32 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let ms = ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000;
    (ms & 0xffff_ffff) as u32
}

/// F1: convert an RDP scancode (with the KBD_EXTENDED 0x100 flag folded in,
/// see desk-backend's InputEvent::Key contract) to an evdev/Linux keycode
/// for zwp_virtual_keyboard_v1.
///
/// The naive `scancode - 8` shortcut drops KBDNUMPAD (0x0800) and KBDSPECIAL
/// (0x0400) keys (numpad Enter, Pause/Break) and mis-maps several extended
/// keys (e.g. 0x11D). With FreeRDP present we route through WinPR's key
/// tables (VK lookup per MS-RDP-IOT, then VK → evdev); otherwise fall back
/// to the legacy linear map so the backend still builds without dev headers.
#[cfg(freerdp_present)]
pub fn rdp_scancode_to_evdev(scancode: u32) -> u32 {
    // Safety: pure table lookup in WinPR's keymap data; no pointers escape.
    let key = unsafe { freerdp_sys::wayrdp_rdp_scancode_to_evdev(scancode) };
    // WinPR's VK→evdev table has holes for the Windows/Menu keys
    // (VK_LWIN/VK_RWIN/VK_APPS arrive as 0) and for the right-hand Alt/Ctrl
    // (VK_RMENU/VK_RCONTROL), so patch those extended scancodes onto the
    // matching evdev keycodes. Super/AltGr are mandatory for Hyprland
    // keybinds and non-US layouts, so they must never be dropped.
    if key == 0 {
        match scancode {
            0x15B => return 125, // LWIN   -> KEY_LEFTMETA
            0x15C => return 126, // RWIN   -> KEY_RIGHTMETA
            0x15D => return 127, // Menu   -> KEY_COMPOSE
            0x11D => return 97,  // RCtrl  -> KEY_RIGHTCTRL
            0x138 => return 100, // RAlt   -> KEY_RIGHTALT (AltGr)
            // Extended nav cluster: WinPR's VK->evdev table has holes for
            // these too (observed live: SUPER+SHIFT+arrows dropped as
            // "unmapped"). Without them Hyprland window-move binds die.
            0x11C => return 96,  // KP Enter -> KEY_KPENTER
            0x135 => return 98,  // KP /     -> KEY_KPSLASH
            0x145 => return 69,  // NumLock  -> KEY_NUMLOCK (KBDSPECIAL)
            0x146 => return 119, // Pause    -> KEY_PAUSE (KBDSPECIAL)
            0x147 => return 102, // Home     -> KEY_HOME
            0x148 => return 103, // Up       -> KEY_UP
            0x149 => return 104, // PgUp     -> KEY_PAGEUP
            0x14B => return 105, // Left     -> KEY_LEFT
            0x14D => return 106, // Right    -> KEY_RIGHT
            0x14F => return 107, // End      -> KEY_END
            0x150 => return 108, // Down     -> KEY_DOWN
            0x151 => return 109, // PgDn     -> KEY_PAGEDOWN
            0x152 => return 110, // Insert   -> KEY_INSERT
            0x153 => return 111, // Delete   -> KEY_DELETE
            _ => {}
        }
    }
    key
}

#[cfg(not(freerdp_present))]
pub fn rdp_scancode_to_evdev(scancode: u32) -> u32 {
    (scancode & !0x100).saturating_sub(8)
}

/// xkb modifier bit for an evdev keycode, if it is a modifier key.
/// Values match wl_keyboard/xkb mask bits (Shift=1<<0 .. Logo=1<<6).
pub fn evdev_mod_bit(key: u32) -> Option<u32> {
    Some(match key {
        42 | 54 => 1,    // KEY_LEFTSHIFT | KEY_RIGHTSHIFT
        29 | 97 => 4,    // KEY_LEFTCTRL | KEY_RIGHTCTRL
        56 | 100 => 8,   // KEY_LEFTALT | KEY_RIGHTALT
        125 | 126 => 64, // KEY_LEFTMETA | KEY_RIGHTMETA (Super)
        _ => return None,
    })
}
