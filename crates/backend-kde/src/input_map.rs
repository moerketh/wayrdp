//! Pure RDP → EIS input mapping, unit-tested without a compositor.

use desk_backend::{InputEvent, PointerButton};

/// evdev button code for an RDP pointer button. Same codes as
/// backend-wlroots so both backends behave alike.
pub fn button_code(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => 0x110,    // BTN_LEFT
        PointerButton::Right => 0x111,   // BTN_RIGHT
        PointerButton::Middle => 0x112,  // BTN_MIDDLE
        PointerButton::Back => 0x116,    // BTN_BACK
        PointerButton::Forward => 0x115, // BTN_FORWARD
    }
}

/// RDP wheel deltas → EIS discrete scroll (`ei_scroll.scroll_discrete`).
/// Both use 120 units per notch, but RDP's positive is wheel-up / left and
/// EIS's positive is down / right, so both axes flip sign.
pub fn scroll_discrete(dx: i32, dy: i32) -> (i32, i32) {
    (-dx, -dy)
}

/// TS_SYNC_* lock bits (desk-backend `InputEvent::LockKeys`).
pub const SYNC_SCROLL: u8 = 1;
pub const SYNC_NUM: u8 = 2;
pub const SYNC_CAPS: u8 = 4;

/// evdev keycodes of the lock keys.
pub const KEY_CAPSLOCK: u32 = 58;
pub const KEY_NUMLOCK: u32 = 69;
pub const KEY_SCROLLLOCK: u32 = 70;

/// Lock state as the compositor reports it (`ei_keyboard.modifiers`
/// locked mask, resolved through the keymap's modifier indices).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Locks {
    pub caps: bool,
    pub num: bool,
    pub scroll: bool,
}

/// Lock keys to tap (press + release) so the compositor's lock state matches
/// the client's. `None` (state not yet reported) → tap nothing: toggling
/// blind would invert whatever the real state is.
pub fn lock_taps(current: Option<Locks>, wanted_bits: u8) -> Vec<u32> {
    let Some(cur) = current else {
        return Vec::new();
    };
    let mut taps = Vec::new();
    if cur.caps != (wanted_bits & SYNC_CAPS != 0) {
        taps.push(KEY_CAPSLOCK);
    }
    if cur.num != (wanted_bits & SYNC_NUM != 0) {
        taps.push(KEY_NUMLOCK);
    }
    if cur.scroll != (wanted_bits & SYNC_SCROLL != 0) {
        taps.push(KEY_SCROLLLOCK);
    }
    taps
}

/// Clamp a session-space pointer position into an EIS absolute region
/// (`x, y, width, height` in logical pixels, plus its scale) and translate
/// it to the region's coordinate space.
pub fn region_position(x: i32, y: i32, region: (u32, u32, u32, u32, f64)) -> (f32, f32) {
    let (rx, ry, rw, rh, scale) = region;
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let max_x = (rw.max(1) - 1) as f64;
    let max_y = (rh.max(1) - 1) as f64;
    let lx = (x as f64 / scale).clamp(0.0, max_x) + rx as f64;
    let ly = (y as f64 / scale).clamp(0.0, max_y) + ry as f64;
    (lx as f32, ly as f32)
}

/// Collapse runs of queued pointer motions to their newest position, never
/// across another event: a click must land where the pointer was when the
/// user clicked. A slow drain then costs no extra motion events (the cursor
/// still ends up in the right place) instead of replaying every one.
pub fn coalesce_motion(events: Vec<InputEvent>) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = Vec::with_capacity(events.len());
    for ev in events {
        if matches!(ev, InputEvent::PointerMotion { .. })
            && matches!(out.last(), Some(InputEvent::PointerMotion { .. }))
        {
            *out.last_mut().expect("checked") = ev;
        } else {
            out.push(ev);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_match_backend_wlroots() {
        assert_eq!(button_code(PointerButton::Left), 0x110);
        assert_eq!(button_code(PointerButton::Right), 0x111);
        assert_eq!(button_code(PointerButton::Middle), 0x112);
        assert_eq!(button_code(PointerButton::Back), 0x116);
        assert_eq!(button_code(PointerButton::Forward), 0x115);
    }

    #[test]
    fn wheel_up_scrolls_up() {
        // One notch up (RDP +120) is EIS -120 (negative = up).
        assert_eq!(scroll_discrete(0, 120), (0, -120));
        assert_eq!(scroll_discrete(-240, -120), (240, 120));
    }

    #[test]
    fn lock_taps_only_for_mismatches_and_never_blind() {
        assert!(lock_taps(None, SYNC_CAPS).is_empty());
        let off = Locks::default();
        assert_eq!(lock_taps(Some(off), 0), Vec::<u32>::new());
        assert_eq!(
            lock_taps(Some(off), SYNC_CAPS | SYNC_NUM),
            vec![KEY_CAPSLOCK, KEY_NUMLOCK]
        );
        let caps_on = Locks {
            caps: true,
            ..Locks::default()
        };
        assert_eq!(lock_taps(Some(caps_on), SYNC_CAPS), Vec::<u32>::new());
        assert_eq!(
            lock_taps(Some(caps_on), SYNC_SCROLL),
            vec![KEY_CAPSLOCK, KEY_SCROLLLOCK]
        );
    }

    #[test]
    fn region_position_clamps_and_offsets() {
        let r = (0, 0, 1366, 768, 1.0);
        assert_eq!(region_position(100, 200, r), (100.0, 200.0));
        assert_eq!(region_position(-5, 9_999, r), (0.0, 767.0));
        let off = (1920, 0, 1024, 768, 1.0);
        assert_eq!(region_position(10, 10, off), (1930.0, 10.0));
        let hidpi = (0, 0, 960, 540, 2.0);
        assert_eq!(region_position(1000, 500, hidpi), (500.0, 250.0));
    }

    #[test]
    fn motion_runs_collapse_but_not_across_clicks() {
        let m = |x| InputEvent::PointerMotion { x, y: 0 };
        let click = InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: true,
        };
        let out = coalesce_motion(vec![m(1), m(2), m(3), click, m(4), m(5)]);
        let xs: Vec<String> = out.iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(
            xs,
            vec![
                format!("{:?}", m(3)),
                format!(
                    "{:?}",
                    InputEvent::PointerButton {
                        button: PointerButton::Left,
                        pressed: true
                    }
                ),
                format!("{:?}", m(5)),
            ]
        );
    }
}
