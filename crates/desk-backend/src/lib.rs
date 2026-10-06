//! The compositor-agnostic backend contract for wayrdp.
//!
//! Everything compositor-specific (virtual outputs, PipeWire, Plasma heal
//! logic, portal consent) lives behind these traits. rdp-core only ever
//! calls `open` once per peer, `resize` on Display-Control changes, and
//! `inject` for input PDUs — and waits for frames.

use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("backend unavailable on this session: {0}")]
    Unavailable(String),
    #[error("operation not supported by this backend: {0}")]
    Unsupported(&'static str),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, BackendError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra32,
    Rgba32,
}

#[derive(Debug)]
pub struct FrameMeta {
    pub size: Size,
    pub stride: usize,
    pub format: PixelFormat,
    pub damage: Vec<Rect>,
    /// CLOCK_MONOTONIC milliseconds (u32-truncated, wraps) at which the
    /// backend finished capturing this frame, for latency instrumentation.
    /// 0 = unstamped (old plugins / backends that don't time it).
    pub capture_stamp_ms: u32,
}

/// Pixel storage behind a shared-deref indirection so the SHM-plugin backend
/// can hand out a zero-copy mmap view (e8) while copied backends wrap an
/// owned `Box<[u8]>`.
pub type FramePixels = std::sync::Arc<dyn std::ops::Deref<Target = [u8]> + Send + Sync>;

/// One delivered frame.
pub struct Frame {
    pub meta: FrameMeta,
    pub pixels: FramePixels,
}

/// Frames flow out of the backend on this channel into the encoder thread.
pub type FrameTx = std::sync::mpsc::Sender<Frame>;

/// Wake hook fired after each frame is sent on the channel. rdp-core uses
/// it to signal a WinPR event that sits in the peer mainloop's wait set, so
/// a freshly-published frame wakes the mainloop immediately instead of
/// waiting out its poll timeout. Callers on the pump thread must keep this
/// cheap and non-blocking — it runs inline with frame delivery.
pub type FrameWake = std::sync::Arc<dyn Fn() + Send + Sync>;

/// Latest cursor position packed as `(x << 32) | y`, in SESSION coordinates
/// (NOT layout coordinates — the client's `PTR_POS_UPDATE` is session-space,
/// so a backend must subtract its output's layout origin before publishing).
/// Shared directly with the peer context via `Arc` so the mainloop can poll
/// it without channel identity concerns: there is exactly one allocation,
/// one `Arc::as_ptr`, and `Relocation` semantics match newest-wins metadata
/// cursor updates. Written by the backend at motion-inject cadence, read by
/// the peer mainloop each frame.
pub type PointerPositionSlot = std::sync::Arc<std::sync::atomic::AtomicU64>;

#[derive(Debug, Clone)]
pub enum CaptureTarget {
    /// Backend creates a virtual output at exactly this size (KDE, mutter,
    /// wlroots-headless).
    NewVirtualOutput(Size),
    /// Just capture an existing output (generic portal fallback; no resize).
    ExistingOutput { id: String },
}

#[derive(Debug, Clone)]
pub struct SessionRequest {
    pub initial_size: Size,
    pub target: CaptureTarget,
    /// RDP KeyboardLayout (LCID, e.g. 0x0409 = en-US) the client advertised
    /// in TS_UD_CS_CORE; 0 when unknown. Backends that drive a real
    /// compositor keyboard should map this to their XKB layout so the guest
    /// produces what the user's keycaps say.
    pub keyboard_layout: u32,
    /// Client-side desktop scale in percent (TS_UD_CS_CORE
    /// desktopScaleFactor; 100 = no scaling). 0 when the client didn't
    /// advertise one. HiDPI-host clients (vmconnect on a 4K laptop) send
    /// 125/150/…; compositors that support output scale can use it to keep
    /// text at the physical size the user expects.
    pub desktop_scale_pct: u32,
}

impl SessionRequest {
    /// For tests and synthetic sessions that don't carry a client.
    pub fn without_client(size: Size) -> Self {
        Self {
            initial_size: size,
            target: CaptureTarget::NewVirtualOutput(size),
            keyboard_layout: 0,
            desktop_scale_pct: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone)]
pub enum InputEvent {
    PointerMotion {
        x: i32,
        y: i32,
    },
    PointerButton {
        button: PointerButton,
        pressed: bool,
    },
    /// Wheel / horizontal-wheel deltas in RDP rotation units (positive
    /// `dy` = wheel up), as decoded from PTR_FLAGS_WHEEL; backends map them
    /// onto whatever mechanism the compositor offers (wlroots axis, ...).
    PointerAxis {
        dx: i32,
        dy: i32,
    },
    /// Hardware scancode = the RDP code field with the KBD_EXTENDED (0x100)
    /// flag folded in. Backends must translate this field to compositor
    /// keycodes through WinPR's key tables (scancode → VK → evdev; see
    /// backend-wlroots' F1 path) — the linear `-8` shortcut drops
    /// KBDSPECIAL/KBDNUMPAD keys and mis-maps extended keys.
    Key {
        scancode: u32,
        pressed: bool,
    },
    /// Decoded UCS-2 code point from TS_UNICODE_KEYBOARD_EVENT.
    Unicode(char),
    /// Client lock-key state from TS_SYNC_EVENT_PDU (`peer_input_sync`):
    /// TS_SYNC_* bit flags (SCROLL_LOCK=1, NUM_LOCK=2, CAPS_LOCK=4,
    /// KANA_LOCK=8). Backends should map them onto the "locked" modifier
    /// group rather than synthesizing key presses: injecting releases for
    /// the lock keys themselves would toggle the state twice. Clients that
    /// never send sync events (xfreerdp) simply never emit this.
    LockKeys {
        lock_bits: u8,
    },
}

/// An input event plus the CLOCK_MONOTONIC millisecond stamp (u32, wraps)
/// at which the RDP peer decoded it from the wire. Used for latency
/// instrumentation: backends compare it to the drain time to measure the
/// pump's input-queue latency. Stamps are informational only — never make
/// correctness decisions on them.
#[derive(Debug, Clone)]
pub struct StampedInput {
    pub event: InputEvent,
    /// 0 = unstamped (tests, synthetic producers).
    pub stamp_ms: u32,
}

pub trait DesktopBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Whether `resize` will actually change the captured output. When false,
    /// rdp-core should not advertise full Display-Control resize to clients.
    fn supports_dynamic_resize(&self) -> bool;

    fn open(&self, req: SessionRequest) -> Result<Box<dyn CaptureSession>>;
}

pub trait CaptureSession: Send {
    fn start(&mut self, tx: FrameTx) -> Result<()>;

    /// Logical resize. Settle timing, containment healing, and stale-cache
    /// handling are the backend's job — implementations block until the new
    /// output is producing frames or a bounded timeout passes.
    fn resize(&mut self, size: Size) -> Result<()>;

    fn inject(&mut self, input: StampedInput) -> Result<()>;

    /// Optional cursor-metadata slot: the backend publishes the latest
    /// session-space cursor position here on every injected motion; rdp-core
    /// reads it from the mainloop and relays it to the client as a
    /// `PTR_POS_UPDATE`. Uses a single shared `AtomicU64` —
    /// position updates are idempotent and newest-wins, so dropping
    /// intermediates is a feature, and there is no pair-identity to break.
    /// Default `None` = painted video cursor only.
    ///
    /// NOTE: this is half of Metadata mode. Until the actual cursor SHAPE is
    /// also relayed (and the compositor's own cursor hidden on the captured
    /// output), rdp-core only installs a slot when `WAYRDP_POINTER_METADATA`
    /// is set — FreeRDP-based clients warp the LOCAL cursor on
    /// PTR_POS_UPDATE, so echoing injected motion back at 50-100 ms latency
    /// would make the xfreerdp cursor fight the user.
    fn set_pointer_position_slot(&mut self, _slot: PointerPositionSlot) {}

    /// Optional frame-wake hook: rdp-core installs this before `start()`.
    /// The backend calls it right after each frame send so the consumer's
    /// wait set wakes immediately (e6). Default no-op = consumer polls.
    fn set_frame_wake(&mut self, _wake: FrameWake) {}

    /// Upper bound for how long `inject` may block before unwinding: KWin
    /// pipe-fills use a 50 ms wait (hyperv-rdp-extras), a hanging compositor
    /// must not wedge the RDP input thread. Default: 100 ms.
    fn inject_deadline(&self) -> Duration {
        Duration::from_millis(100)
    }
}
