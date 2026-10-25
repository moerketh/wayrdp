//! wlroots-class backend for wayrdp — Hyprland / Sway.
//!
//! This is the structural answer to the "Hyprland has no KWin-style virtual
//! output" gap. Hyprland exposes no
//! `zkde_screencast_unstable_v1`; instead we drive the compositor's own
//! native Wayland protocols directly — same zero-copy-capable intent as the
//! KWin path, no portal consent.
//!
//!   * **Virtual output** — `hyprctl output create headless <name>` at the
//!     exact client-negotiated size. Sizing a dedicated headless output to
//!     the client eliminates the leftover black bar that a fixed-mode
//!     physical output leaves at non-native client resolutions.
//!   * **Capture** — `ext_image_copy_capture_manager_v1` sourcing an
//!     `ext_output_image_capture_source_v1` bound to the headless
//!     `wl_output`. Hyprland's zero-copy-capable path (dmabuf when the
//!     compositor advertises it). This cut reads back shm buffers, which
//!     every compositor with the staging protocol provides; portal screencast
//!     remains the fallback when the protocol is absent.
//!   * **Input** — `zwlr_virtual_pointer_manager_v1` +
//!     `zwp_virtual_keyboard_manager_v1`: direct injection, no portal / EIS
//!     consent round trip.
//!
//! All Wayland I/O lives on one dedicated pump thread driving a manual
//! `EventQueue` + `poll(2)` loop (the extras idiom — no calloop, no
//! pw_thread_loop). `open` is I/O-free so rdp-core can call it on the peer
//! thread; the connection and virtual output are established in `start`.
//! `resize`/`inject` are thread-safe `SyncSender` commands drained by the
//! loop between dispatches.

use crossbeam_channel::{Receiver, TryRecvError, TrySendError};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::process::Command;
use std::sync::{Arc, Mutex};

use desk_backend::stats::LatStats;
use desk_backend::{
    BackendError, CaptureSession, CaptureTarget, DesktopBackend, Frame, FrameMeta, FramePixels,
    FrameTx, InputEvent, PixelFormat, PointerButton, Rect, Result, SessionRequest, Size,
};

use wayland_client::event_created_child;
use wayland_client::protocol::{wl_buffer, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::{
    self, ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_cursor_session_v1::{
    self, ExtImageCopyCaptureCursorSessionV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self, ExtImageCopyCaptureFrameV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    self, ExtImageCopyCaptureManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self, ExtImageCopyCaptureSessionV1,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

const OUTPUT_PREFIX: &str = "wayrdp";

/// Per-session sequence for unique output names (wayrdp-1, wayrdp-2, ...).
/// Overlapping sessions (fast reconnects) must never share an output name:
/// session A's exit-time `output remove` would otherwise delete the output
/// that session B's capture session is bound to, poisoning B's wl_output
/// ("invalid output resource") and silently killing its capture and input.
static OUTPUT_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// At most one capture pump may own the headless-output lifecycle at a time.
/// Hyprland 0.56.2 crashes (SIGABRT in `CImageCopyCaptureFrame` →
/// `CScreenshareFrame::transform`, upstream hyprwm/Hyprland#16317) when an
/// output is removed while another client's ext-image-copy-capture session
/// has a frame in flight on it. A reconnecting RDP client otherwise does
/// exactly that: the new session's pump removes the previous pump's live
/// output. The flag is released by the pump's own teardown, so output
/// removal always happens while its only capture session is quiescent.
static PUMP_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Outputs owned by a live pump in THIS process, name-exact. The pump-lease
/// timeout path proceeds without PUMP_ACTIVE, so "flag clear" can no longer
/// mean "no live wayrdp output exists" — a later lease-holder's
/// cleanup_stale_outputs would then remove that uncounted pump's live output
/// (poisoning its wl_output and, with a capture frame in flight, triggering
/// the documented Hyprland SIGABRT). The sweep consults this registry and
/// never removes a name a live pump owns, regardless of lease state.
static LIVE_OUTPUTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Reserve an output name for this pump BEFORE the create, so no concurrent
/// lease-holder's sweep can race the create→register window.
fn live_output_add(name: &str) {
    let mut live = LIVE_OUTPUTS.lock().unwrap_or_else(|p| p.into_inner());
    if !live.iter().any(|n| n == name) {
        live.push(name.to_string());
    }
}

/// Release the name at teardown; called before/with destroy_headless_output
/// on every path that ends this pump's ownership of the output.
fn live_output_remove(name: &str) {
    LIVE_OUTPUTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|n| n != name);
}

/// Resolve the signature of the *live* Hyprland instance.
///
/// wayrdp shells out to `hyprctl`, which locates the compositor via
/// `HYPRLAND_INSTANCE_SIGNATURE`. That var only exists if the systemd --user
/// service imported it from the session that started wayrdp. After a
/// compositor restart (sddm re-login) or a bare `systemctl --user restart
/// wayrdp`, the inherited value is either absent or points at a dead
/// instance's `.socket.sock`, so every `hyprctl` child exits 4 ("Couldn't
/// connect to .../.socket.sock") and no headless output ever materializes.
///
/// When the env var is unset or its socket is gone, scan
/// `$XDG_RUNTIME_DIR/hypr/` for the newest directory that actually has a
/// `.socket.sock` — that is the running instance. Returns None when nothing
/// qualifies (leave the child's env alone; hyprctl will error as before).
fn current_hyprland_signature() -> Option<String> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from)?;
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(rt.join("hypr")).ok()? {
        // One unreadable entry must not abort the whole scan.
        let Ok(entry) = entry else { continue };
        let sock = entry.path().join(".socket.sock");
        // Skip orphaned sockets (respawn leftovers). Liveness, not presence.
        if std::os::unix::net::UnixStream::connect(&sock).is_err() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let replace = best.as_ref().map_or(true, |(t, _)| mtime > *t);
        if replace {
            best = Some((mtime, name));
        }
    }
    best.map(|(_, name)| name)
}

/// Probe whether an instance signature's IPC socket is actually connectable.
///
/// `Path::exists()` is NOT sufficient: a compositor respawn leaves the old
/// instance's `.socket.sock` file on disk (orphaned, no listener), so the
/// stale signature "passes" a presence check while every connect fails with
/// ENOTCONN/ECONNREFUSED — which is exactly the `Couldn't connect to
/// .../.socket.sock (4)` churn that kept headless mode from ever applying.
/// A short connect is the only honest liveness test.
fn socket_live(rt: &std::ffi::OsStr, sig: &str) -> bool {
    let Some(rt) = rt.to_str() else { return false };
    let path = std::path::PathBuf::from(rt)
        .join("hypr")
        .join(sig)
        .join(".socket.sock");
    if !path.exists() {
        return false;
    }
    // Non-blocking-ish probe: a live Hyprland IPC listener accepts instantly.
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

/// Build a `hyprctl` child whose env names the *current* instance. Honors an
/// already-valid inherited signature; otherwise sets the discovered one so
/// restart-safe operation never depends on the service's launch-time env.
fn hyprctl() -> Command {
    let mut cmd = Command::new("hyprctl");
    let rt = std::env::var_os("XDG_RUNTIME_DIR");
    let inherited = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .ok()
        .filter(|s| !s.is_empty())
        .filter(|sig| rt.as_deref().map_or(false, |rt| socket_live(rt, sig)));
    match inherited.or_else(current_hyprland_signature) {
        Some(sig) => {
            if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().as_deref() != Some(sig.as_str()) {
                cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig);
            }
        }
        None => {} // nothing found; hyprctl errors as it would anyway
    }
    cmd
}

/// Single choke point for running hyprctl. Times the call; any invocation
/// taking over 2s is logged. A hung hyprctl under churn would show up here as
/// a slow-call storm rather than an invisible wedge; if the marker ever fires,
/// the 3s kill deadline becomes a spawn+try_wait loop inside this one function.
/// `.status()` callers became hyprctl_run(..).map(|o| o.status): hyprctl's
/// stdout (usually "ok") is now captured instead of spamming the journal.
fn hyprctl_run(args: &[&str]) -> std::io::Result<std::process::Output> {
    let start = std::time::Instant::now();
    let out = hyprctl().args(args).output();
    let ms = start.elapsed().as_millis();
    if ms > 2000 {
        eprintln!(
            "[backend-wlroots] hyprctl {} took {} ms",
            args.join(" "),
            ms
        );
    }
    out
}

struct PumpLease;
impl Drop for PumpLease {
    fn drop(&mut self) {
        PUMP_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        // Teardown anchor: the previous pump has fully unwound (capture
        // session, virtual pointer/keyboard, and headless-output remove are
        // all done by the time this lease drops). The A8 outcome gate reads
        // this line as "the killed peer's pump released the lease".
        eprintln!("[backend-wlroots] teardown: pump lease released");
    }
}

/// Claim the pump lease; returns the lease on success, or None if a previous
/// pump is stuck. Holding the lease and "the flag is set by me" are the same
/// fact: only the holder's Drop may clear PUMP_ACTIVE, so a timed-out caller
/// must proceed WITHOUT a guard (otherwise its teardown would release a
/// third pump's lease while that pump is running).
///
/// compare_exchange (not load-then-swap) is what actually serializes: two
/// waiters racing on a just-released flag cannot both win.
fn claim_pump() -> Option<PumpLease> {
    let start = std::time::Instant::now();
    loop {
        if PUMP_ACTIVE
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            return Some(PumpLease);
        }
        if start.elapsed() > std::time::Duration::from_secs(20) {
            eprintln!("[backend-wlroots] previous capture pump still active after 20s; continuing without the lease (no stale-output cleanup, no teardown clear)");
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn next_output_name() -> String {
    let n = OUTPUT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    format!("{OUTPUT_PREFIX}-{n}")
}

/// Remove any leftover wayrdp outputs from crashed or hard-killed sessions.
/// Runs once at session start; skips our own name space conflicts by design.
fn cleanup_stale_outputs() {
    let out = hyprctl_run(&["monitors", "all", "-j"]);
    let Ok(out) = out else { return };
    if !out.status.success() {
        return;
    }
    let Ok(mons) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return;
    };
    let Some(mons) = mons.as_array() else { return };
    for m in mons {
        let Some(name) = m.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if name == OUTPUT_PREFIX || name.starts_with(&format!("{OUTPUT_PREFIX}-")) {
            // Never remove an output a live pump in this process owns: the
            // lease-timeout path creates pumps PUMP_ACTIVE cannot see, and
            // the sweep must stay liveness-aware, not lease-assuming.
            if LIVE_OUTPUTS
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .any(|n| n == name)
            {
                eprintln!("[backend-wlroots] skipping live output {name}");
                continue;
            }
            eprintln!("[backend-wlroots] removing stale output {name}");
            let _ = hyprctl_run(&["output", "remove", name]);
        }
    }
}

pub struct WlrBackend;

impl DesktopBackend for WlrBackend {
    fn name(&self) -> &'static str {
        "wlroots"
    }

    fn supports_dynamic_resize(&self) -> bool {
        true
    }

    fn open(&self, req: SessionRequest) -> Result<Box<dyn CaptureSession>> {
        let initial = match req.target {
            CaptureTarget::NewVirtualOutput(s) => s,
            CaptureTarget::ExistingOutput { .. } => req.initial_size,
        };
        let prefs = WlSessionPrefs::from_request(&req);
        Ok(Box::new(WlrCaptureSession::new(even(initial), prefs)))
    }

    fn open_clipboard(&self) -> Option<Box<dyn desk_backend::Clipboard>> {
        Some(Box::new(clipboard_wayland::WlClipboard::new()))
    }
}

/// Per-client presentation prefs carried from `SessionRequest` into the
/// pump: XKB layout/variant for the virtual keyboard and the compositor
/// scale for the headless output.
struct WlSessionPrefs {
    kbd_layout: String,
    kbd_variant: String,
    output_scale: f64,
}

impl WlSessionPrefs {
    fn from_request(req: &SessionRequest) -> Self {
        let (layout, variant) = if req.keyboard_layout == 0 {
            ("us".to_string(), String::new())
        } else {
            let (l, v) = lcid_to_xkb(req.keyboard_layout);
            (l.to_string(), v.to_string())
        };
        // HiDPI passthrough: percent → factor (100→1.0, 125→1.25), clamped
        // to Hyprland's sane range. Unknown (0) and <100 mean 1.0 — the
        // client didn't scale, so we don't either.
        let output_scale = if req.desktop_scale_pct >= 100 {
            (req.desktop_scale_pct as f64 / 100.0).clamp(1.0, 3.0)
        } else {
            1.0
        };
        Self {
            kbd_layout: layout,
            kbd_variant: variant,
            output_scale,
        }
    }
}

fn even(s: Size) -> Size {
    Size {
        width: s.width.max(2) & !1,
        height: s.height.max(2) & !1,
    }
}

/// CLOCK_MONOTONIC milliseconds truncated to u32 (wraps every ~49 days —
/// fine, all deltas use wrapping_sub). Shared clock domain with the capture
/// plugin's header stamp, so cross-component queue latencies are directly
/// comparable. 0 is reserved as "unstamped"; a real stamp of exactly 0 is
/// astronomically unlikely (boot-second boundary) and at worst drops one
/// sample.
fn now_mono_ms_u32() -> u32 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let ms = ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000;
    (ms & 0xffff_ffff) as u32
}

/// WAYRDP_TRACE_MARKER=`dx,dy,rrggbb`: a test page draws a marker of that
/// colour at pointer + (dx, dy) (the T4 probe: `60,0,ff00ff`). The shm pump
/// then times each injected motion until a captured frame shows the marker
/// there — input → visible change, compositor and app included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TraceMarker {
    dx: i32,
    dy: i32,
    rgb: [u8; 3],
}

fn parse_trace_marker(v: &str) -> Option<TraceMarker> {
    let mut it = v.split(',');
    let dx = it.next()?.trim().parse().ok()?;
    let dy = it.next()?.trim().parse().ok()?;
    let hex = it.next()?.trim();
    if it.next().is_some() || hex.len() != 6 {
        return None;
    }
    let c = u32::from_str_radix(hex, 16).ok()?;
    Some(TraceMarker {
        dx,
        dy,
        rgb: [(c >> 16) as u8, (c >> 8) as u8, c as u8],
    })
}

/// Whether the BGRA pixel at (x, y) is the marker colour (±8 per channel).
fn marker_at(pixels: &[u8], stride: usize, size: Size, x: i32, y: i32, rgb: [u8; 3]) -> bool {
    if x < 0 || y < 0 || x as u32 >= size.width || y as u32 >= size.height {
        return false;
    }
    let off = y as usize * stride + x as usize * 4;
    let Some(px) = pixels.get(off..off + 3) else {
        return false;
    };
    let near = |a: u8, b: u8| a.abs_diff(b) <= 8;
    near(px[0], rgb[2]) && near(px[1], rgb[1]) && near(px[2], rgb[0])
}

pub struct WlrCaptureSession {
    size: Mutex<Size>,
    cmd_tx: Mutex<Option<crossbeam_channel::Sender<desk_backend::StampedInput>>>,
    pump: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Latest-wins resize slot shared with the pump thread (F7). Resizes
    /// never enter the command channel: back-to-back requests coalesce to
    /// the newest size instead of stacking protocol churn (each resize
    /// recreates the headless output and the capture session).
    resize_slot: Arc<Mutex<Option<Size>>>,
    /// Direct shared cell for latest pointer position (Metadata-mode relay,
    /// half-built: see the trait docs). One allocation, `Arc::as_ptr`
    /// matches on both ends, newest-wins semantics are native. Kept Option
    /// so backends that don't opt in leave it None.
    pointer_pos_slot: Arc<Mutex<Option<desk_backend::PointerPositionSlot>>>,
    /// Per-client presentation prefs. Fixed at open: mid-session changes
    /// are vanishingly rare and would need a keymap re-upload / monitor
    /// reset we don't do today.
    prefs: WlSessionPrefs,
    /// eventfd that send() bumps after queueing an input event. The shm pump
    /// polls it next to the wayland fd, so an idle→input transition wakes the
    /// pump immediately instead of waiting out the frame-poll timeout. -1 /
    /// None when creation failed: the channel still works, worst case is the
    /// old polling latency.
    input_wake_fd: Mutex<Option<i32>>,
    /// Frame-wake hook fired after each frame send (e6): signals the peer
    /// mainloop's WinPR wait set so a new frame is encoded immediately
    /// instead of at the next 16ms poll tick. None until rdp-core installs.
    frame_wake: Mutex<Option<desk_backend::FrameWake>>,
    /// Client-drawn-cursor slot (WAYRDP_CLIENT_CURSOR), taken by the pump at
    /// start(). Set = capture the cursor separately instead of painting it.
    cursor_slot: Mutex<Option<desk_backend::PointerShapeSlot>>,
}

impl WlrCaptureSession {
    fn new(size: Size, prefs: WlSessionPrefs) -> Self {
        Self {
            size: Mutex::new(size),
            cmd_tx: Mutex::new(None),
            pump: Mutex::new(None),
            resize_slot: Arc::new(Mutex::new(None)),
            pointer_pos_slot: Arc::new(Mutex::new(None)),
            prefs,
            input_wake_fd: Mutex::new(None),
            frame_wake: Mutex::new(None),
            cursor_slot: Mutex::new(None),
        }
    }

    fn send(&self, ev: desk_backend::StampedInput) -> Result<()> {
        let guard = self.cmd_tx.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            // The channel is UNBOUNDED (post–pointer-jerkiness): on the
            // previous 64-slot bounded channel, a heavy capture pump starving
            // on capture+frame-submit made motion try_sends fail and the pump
            // coalesces queued motions to the newest — cursor position then
            // jumped in coarse ~tens-of-ms steps ("jerky mouse"). Bursts are
            // small (mstsc/vmconnect ~125/s) and the pump coalesces on drain;
            // the drain loop below is what bounds memory. Motion and other
            // inputs now share one path: plain try_send, non-blocking; a
            // disconnected channel still surfaces as the pump-exit error.
            Some(tx) => match tx.try_send(ev) {
                Ok(()) => {
                    // Bump the wake fd so a polling pump drains the channel
                    // now, not at its next frame-poll timeout. EINTR/EAGAIN
                    // are ignorable: worst case is one poll timeout of delay.
                    if let Some(fd) = *self.input_wake_fd.lock().unwrap_or_else(|p| p.into_inner())
                    {
                        let one: u64 = 1;
                        unsafe {
                            libc::write(fd, (&one as *const u64).cast::<libc::c_void>(), 8);
                        }
                    }
                    Ok(())
                }
                Err(TrySendError::Disconnected(_)) => Err(BackendError::Other(anyhow::anyhow!(
                    "wlroots capture thread exited"
                ))),
                Err(TrySendError::Full(_)) => {
                    unreachable!("crossbeam unbounded channel never reports Full")
                }
            },
            None => Ok(()),
        }
    }
}

impl CaptureSession for WlrCaptureSession {
    fn start(&mut self, tx: FrameTx) -> Result<()> {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<desk_backend::StampedInput>();
        let size = *self.size.lock().unwrap_or_else(|p| p.into_inner());
        *self.cmd_tx.lock().unwrap_or_else(|p| p.into_inner()) = Some(cmd_tx);
        let resize_slot = self.resize_slot.clone();
        // Take the slot before spawning so the pump owns it. If a caller
        // forgets set_pointer_position_slot, the pump sees None and the
        // cursor stays painted in the video (previous behavior).
        let pointer_pos_slot = self
            .pointer_pos_slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        // e6: take the frame-wake hook so the pump owns it; each frame send
        // fires it to wake the peer mainloop's wait set immediately.
        let frame_wake = self
            .frame_wake
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let cursor_slot = self
            .cursor_slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let prefs = WlSessionPrefs {
            kbd_layout: self.prefs.kbd_layout.clone(),
            kbd_variant: self.prefs.kbd_variant.clone(),
            output_scale: self.prefs.output_scale,
        };
        // Input wake eventfd: send() bumps it after queueing so the pump's
        // poll() returns as soon as the first input lands on an idle
        // connection. Non-blocking + cloexec; failure degrades to polling.
        let wake_fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if wake_fd >= 0 {
            *self.input_wake_fd.lock().unwrap_or_else(|p| p.into_inner()) = Some(wake_fd);
        }
        let pump = std::thread::Builder::new()
            .name("wlroots-capture".into())
            .spawn(move || {
                pump_loop(
                    size,
                    tx,
                    resize_slot,
                    cmd_rx,
                    pointer_pos_slot,
                    prefs,
                    wake_fd,
                    frame_wake,
                    cursor_slot,
                )
            })
            .map_err(|e| BackendError::Other(anyhow::anyhow!("spawn capture thread: {e}")))?;
        *self.pump.lock().unwrap_or_else(|p| p.into_inner()) = Some(pump);
        Ok(())
    }

    fn resize(&mut self, size: Size) -> Result<()> {
        *self.size.lock().unwrap_or_else(|p| p.into_inner()) = even(size);
        *self.resize_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(even(size));
        Ok(())
    }

    fn inject(&mut self, input: desk_backend::StampedInput) -> Result<()> {
        self.send(input)
    }

    /// Metadata-mode hook: rdp-core installs the shared slot here before
    /// `start()` (only when WAYRDP_POINTER_METADATA is set). After that,
    /// each injected motion is mirrored as a session-space position in the
    /// slot; the peer's mainloop relays the newest value to the client as
    /// `PTR_POS_UPDATE`. Default no-op preserves the Painted-mode behavior
    /// for callers that never set a slot.
    fn set_pointer_position_slot(&mut self, slot: desk_backend::PointerPositionSlot) {
        *self
            .pointer_pos_slot
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(slot);
    }

    /// e6: rdp-core installs the frame-wake hook before `start()`. Each
    /// pump fires it after a successful frame send so the peer mainloop's
    /// wait returns immediately; no hook = the old 16ms poll cadence.
    fn set_frame_wake(&mut self, wake: desk_backend::FrameWake) {
        *self.frame_wake.lock().unwrap_or_else(|p| p.into_inner()) = Some(wake);
    }

    /// Client-drawn cursor: the pump stops painting the cursor into frames
    /// and publishes its image to the slot — the ext-image-copy pump through
    /// a pointer cursor session, the shm-plugin pump by asking the capture
    /// plugin to leave Hyprland's cursor out and reading the image the
    /// plugin publishes. A plugin without that support keeps the cursor
    /// painted and the slot empty (the client keeps its transparent pointer).
    fn set_pointer_shape_slot(&mut self, slot: desk_backend::PointerShapeSlot) -> bool {
        *self.cursor_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(slot);
        true
    }
}

impl Drop for WlrCaptureSession {
    fn drop(&mut self) {
        *self.cmd_tx.lock().unwrap_or_else(|p| p.into_inner()) = None;
        if let Some(h) = self.pump.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = h.join();
        }
        // The pump has exited, so nothing reads the fd any more; close it
        // after the join so no poll() is left watching a recycled fd number.
        if let Some(fd) = self
            .input_wake_fd
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pump-thread state and dispatch
// ---------------------------------------------------------------------------

struct State {
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    capture_mgr: Option<ExtImageCopyCaptureManagerV1>,
    source_mgr: Option<ExtOutputImageCaptureSourceManagerV1>,
    vptr_mgr: Option<ZwlrVirtualPointerManagerV1>,
    vkbd_mgr: Option<ZwpVirtualKeyboardManagerV1>,

    outputs: Vec<wl_output::WlOutput>,
    output_names: Vec<(u32, String)>, // (wl_output protocol id, compositor name)

    frame_size: Size,
    shm_fmt: Option<wl_shm::Format>, // negotiated shm format (Argb8888/Xrgb8888)
    buffer_ready: bool,              // frame delivered by compositor
    failed: bool,
    damage: Vec<Rect>, // frame damage from the current capture

    // Live capture plumbing (pump-thread owned).
    session: Option<ExtImageCopyCaptureSessionV1>,
    source: Option<ExtImageCaptureSourceV1>,
    pool: Option<ShmBuffer>,

    /// This session's own headless output name (wayrdp-N). Unique per session
    /// so a dying session's teardown cannot remove a live session's output.
    output_name: String,

    vptr: Option<ZwlrVirtualPointerV1>,
    /// True when the pointer manager was bound at version >= 2, i.e. the
    /// `axis_discrete` request is legal (F8).
    vptr_v2: bool,
    vkbd: Option<ZwpVirtualKeyboardV1>,
    /// The compositor rejects zwp_virtual_keyboard_v1.key before a keymap has
    /// been set. Clients (mstsc) may inject keys immediately on connect, so we
    /// hold them until the first keymap has been uploaded, then flush.
    /// Set by the pump loop when the session's cmd channel has hung up
    /// (peer disconnected / session dropped). Breaks the main loop and runs
    /// teardown (destroy_headless_output) instead of blocking forever in
    /// dispatch on an idle desktop.
    shutdown: bool,
    keymap_set: bool,
    /// xkb layout+variant for the virtual keyboard, from the client's RDP
    /// KeyboardLayout (lcid_to_xkb in pump_loop). Written once before the
    /// keymap compiles; default "us"/"" matches legacy behavior.
    kbd_layout: String,
    kbd_variant: String,
    /// Compositor scale for our headless output (from the client's RDP
    /// desktopScaleFactor, percent→factor). 1.0 = legacy behavior.
    output_scale: f64,
    key_buffer: Vec<(u32, bool)>,
    /// xkb depressed-modifier mask of injected keys (SHIFT=1/CTRL=4/ALT=8/
    /// LOGO=64 bits). Hyprland matches keybinds against a *virtual*
    /// keyboard's modifier state, which only `modifiers()` updates — plain
    /// `key()` events leave it at 0, so Super+chords silently never fire.
    mod_mask: u32,
    /// Evdev keycodes currently held, so client autorepeat (mstsc floods
    /// held modifiers like SUPER with synthetic press events) is collapsed
    /// to a single press. Unfiltered repeats land compositor-side as fresh
    /// key presses mid-grab — Hyprland aborts an in-progress SUPER+drag on
    /// the next SUPER press ("drag cut short"). Releases always pass so a
    /// dropped press can't wedge a key.
    keys_down: std::collections::HashSet<u32>,
    /// Client lock-key state (TS_SYNC_NUM_LOCK=2 / CAPS_LOCK=4 bits —
    /// 1:1 with RDP's toggleFlags). Fed to the "locked" modifier group so
    /// RDP sync events survive; we inject releases for the lock keys
    /// themselves, so this is the only path remote Caps/Num Lock changes
    /// reach the compositor. Updated both by InputEvent::LockKeys and by
    /// remote lock-key presses, which mstsc doesn't echo as sync events.
    lock_bits: u8,
    /// Wayland outputs share one coordinate layout; virtual-pointer absolute
    /// motion is specified in an *extent* that the compositor normalizes over
    /// the whole layout bounding box, not per-output. Feeding (x, y, frame_w,
    /// frame_h) while any other output is enabled lands the cursor to the
    /// right of the actual client position (observed: second painted cursor
    /// ~25% off when the Hyper-V console output Virtual-1 shared the layout).
    /// Track the real layout extent + our output's origin so injected coords
    /// map exactly. Zero extent = unknown → fall back to frame-size extent
    /// (correct only when ours is the sole output).
    layout_extent: (u32, u32),
    layout_origin: (i32, i32),
    /// Client-drawn-cursor slot (Metadata mode, env-gated by rdp-core):
    /// newest injected motion is published here in SESSION coordinates so
    /// rdp-core can forward it as `PTR_POS_UPDATE`. `None` means the client
    /// sees the cursor painted in the video (Painted mode).
    pointer_pos_slot: Option<desk_backend::PointerPositionSlot>,
    /// The seat advertises the pointer capability: wl_seat.get_pointer is a
    /// protocol error otherwise, and a cursor session needs a wl_pointer.
    seat_pointer: bool,
    cursor: CursorCapture,
}

impl Default for State {
    fn default() -> Self {
        Self {
            shm: None,
            seat: None,
            capture_mgr: None,
            source_mgr: None,
            vptr_mgr: None,
            vkbd_mgr: None,
            outputs: Vec::new(),
            output_names: Vec::new(),
            frame_size: Size {
                width: 0,
                height: 0,
            },
            shm_fmt: None,
            buffer_ready: false,
            failed: false,
            damage: Vec::new(),
            session: None,
            source: None,
            pool: None,
            output_name: String::new(),
            vptr: None,
            vptr_v2: false,
            vkbd: None,
            keymap_set: false,
            kbd_layout: "us".into(),
            kbd_variant: String::new(),
            output_scale: 1.0,
            shutdown: false,
            key_buffer: Vec::new(),
            mod_mask: 0,
            keys_down: std::collections::HashSet::new(),
            lock_bits: 0,
            layout_extent: (0, 0),
            layout_origin: (0, 0),
            pointer_pos_slot: None,
            seat_pointer: false,
            cursor: CursorCapture::default(),
        }
    }
}

/// Minimum spacing between cursor captures. The compositor may hold a cursor
/// frame until the image changes, but nothing obliges it to: this caps an
/// eager compositor at ~30 captures/s of a tiny buffer.
const CURSOR_CAPTURE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

/// User data of the cursor's capture session and frames, so their events
/// stay apart from the output capture's (user data `()`).
struct CursorTag;

/// Client-drawn cursor (WAYRDP_CLIENT_CURSOR): a pointer cursor session on
/// our output's capture source. The output session is then created without
/// PaintCursors, and the cursor image copied here is published to rdp-core,
/// which sends it as the client's pointer shape. Without a slot (or when the
/// session cannot be created) the cursor stays painted in the video.
#[derive(Default)]
struct CursorCapture {
    slot: Option<desk_backend::PointerShapeSlot>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_session: Option<ExtImageCopyCaptureCursorSessionV1>,
    session: Option<ExtImageCopyCaptureSessionV1>,
    /// The frame in flight; at most one per session.
    frame: Option<ExtImageCopyCaptureFrameV1>,
    size: Option<Size>,
    fmt: Option<wl_shm::Format>,
    pool: Option<(ShmBuffer, Size, wl_shm::Format)>,
    ready: bool,
    failed: bool,
    stopped: bool,
    /// The cursor image intersects our output.
    inside: bool,
    hotspot: (i32, i32),
    next_capture: Option<std::time::Instant>,
    failures: u32,
    last: Option<desk_backend::CursorUpdate>,
}

impl CursorCapture {
    /// Release the cursor session and its in-flight frame. Called before the
    /// capture source or our output goes away: an output removed under a
    /// live frame aborts Hyprland (see claim_pump).
    fn release_session(&mut self) {
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
        if let Some(s) = self.session.take() {
            s.destroy();
        }
        if let Some(c) = self.cursor_session.take() {
            c.destroy();
        }
        self.size = None;
        self.fmt = None;
        self.pool = None;
        self.ready = false;
        self.failed = false;
        self.stopped = false;
        self.inside = false;
        self.next_capture = None;
        self.failures = 0;
    }

    /// Create the cursor session on `source`. Returns false (cursor stays
    /// painted) without a slot, without a pointer-capable seat, or without
    /// the protocol.
    fn bind(
        &mut self,
        state_seat: Option<&wl_seat::WlSeat>,
        seat_pointer: bool,
        capture_mgr: &ExtImageCopyCaptureManagerV1,
        source: &ExtImageCaptureSourceV1,
        qh: &QueueHandle<State>,
    ) -> bool {
        self.release_session();
        if self.slot.is_none() {
            return false;
        }
        let Some(seat) = state_seat.filter(|_| seat_pointer) else {
            eprintln!("[backend-wlroots] client cursor: seat has no pointer; cursor stays painted");
            return false;
        };
        let pointer = self
            .pointer
            .get_or_insert_with(|| seat.get_pointer(qh, ()))
            .clone();
        let cursor_session = capture_mgr.create_pointer_cursor_session(source, &pointer, qh, ());
        self.session = Some(cursor_session.get_capture_session(qh, CursorTag));
        self.cursor_session = Some(cursor_session);
        true
    }

    /// Publish `update` when it differs from what the client has. Returns
    /// true when something was published.
    fn publish(&mut self, update: desk_backend::CursorUpdate) -> bool {
        if self.last.as_ref() == Some(&update) {
            return false;
        }
        if let Some(slot) = &self.slot {
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(update.clone());
        }
        if std::env::var_os("WAYRDP_DEBUG_CURSOR").is_some() {
            match &update {
                desk_backend::CursorUpdate::Shape(img) => eprintln!(
                    "[backend-wlroots] cursor shape {}x{} hot {},{}",
                    img.width, img.height, img.hot_x, img.hot_y
                ),
                desk_backend::CursorUpdate::Hidden => eprintln!("[backend-wlroots] cursor hidden"),
            }
        }
        self.last = Some(update);
        true
    }

    /// One pump-loop step: turn a ready frame into a shape, recover from a
    /// failed one, and keep one capture in flight while the cursor is on our
    /// output. Returns true when a new cursor state was published.
    fn service(&mut self, shm: &wl_shm::WlShm, qh: &QueueHandle<State>) -> bool {
        let Some(session) = self.session.clone() else {
            return false;
        };
        let now = std::time::Instant::now();
        let mut published = false;

        if self.stopped {
            // The compositor ended the session (no cursor can be captured any
            // more). Nothing paints the cursor now, so hide the client one
            // rather than leave a stale shape.
            eprintln!("[backend-wlroots] client cursor: session stopped");
            self.release_session();
            return self.publish(desk_backend::CursorUpdate::Hidden);
        }
        if self.ready {
            self.ready = false;
            self.failures = 0;
            if let Some((pool, size, fmt)) = &self.pool {
                let img = cursor_image_from_shm(&pool.read_pixels(), *size, *fmt, self.hotspot);
                published |= self.publish(desk_backend::CursorUpdate::Shape(Arc::new(img)));
            }
            self.next_capture = Some(now + CURSOR_CAPTURE_INTERVAL);
        }
        if self.failed {
            // Usually new buffer constraints (the cursor image changed size):
            // reallocate on the next capture, backing off if it keeps failing.
            self.failed = false;
            self.pool = None;
            self.failures = self.failures.saturating_add(1);
            self.next_capture = Some(now + CURSOR_CAPTURE_INTERVAL * self.failures.min(30));
        }
        if !self.inside {
            if self.last.is_some() {
                published |= self.publish(desk_backend::CursorUpdate::Hidden);
            }
            return published;
        }
        if self.frame.is_some() || self.next_capture.is_some_and(|t| now < t) {
            return published;
        }
        let (Some(size), Some(fmt)) = (self.size, self.fmt) else {
            return published;
        };
        if size.width == 0 || size.height == 0 {
            return published;
        }
        if self
            .pool
            .as_ref()
            .is_none_or(|(_, s, f)| *s != size || *f != fmt)
        {
            self.pool = match ShmBuffer::create(shm, qh, size, fmt) {
                Ok(p) => Some((p, size, fmt)),
                Err(e) => {
                    eprintln!("[backend-wlroots] client cursor: shm alloc failed: {e}");
                    self.failed = true;
                    return published;
                }
            };
        }
        if let Some((pool, _, _)) = &self.pool {
            let frame = session.create_frame(qh, CursorTag);
            frame.attach_buffer(&pool.buffer);
            frame.capture();
            self.frame = Some(frame);
        }
        published
    }

    /// How long the pump may sleep before the next cursor capture is due.
    fn wait_ms(&self) -> Option<i32> {
        if self.session.is_none() || !self.inside || self.frame.is_some() {
            return None;
        }
        let due = self.next_capture?;
        let left = due.saturating_duration_since(std::time::Instant::now());
        Some(left.as_millis().min(i32::MAX as u128) as i32)
    }
}

/// Convert a captured cursor buffer (wl_shm ARGB8888: B,G,R,A bytes,
/// premultiplied alpha) to the straight-alpha BGRA image rdp-core encodes.
/// XRGB8888 carries no alpha, so its pixels come out opaque. The hotspot is
/// clamped into the image.
fn cursor_image_from_shm(
    px: &[u8],
    size: Size,
    fmt: wl_shm::Format,
    hotspot: (i32, i32),
) -> desk_backend::CursorImage {
    let (w, h) = (size.width as usize, size.height as usize);
    let mut bgra = vec![0u8; w * h * 4];
    let opaque = fmt != wl_shm::Format::Argb8888;
    for (dst, src) in bgra
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(px.as_chunks::<4>().0)
    {
        let a = if opaque { 255 } else { src[3] };
        if a == 0 {
            continue; // fully transparent: leave zeroed
        }
        for (d, &s) in dst.iter_mut().zip(&src[..3]) {
            *d = if a == 255 {
                s
            } else {
                ((u32::from(s) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8
            };
        }
        dst[3] = a;
    }
    let clamp = |v: i32, max: u32| v.clamp(0, max.saturating_sub(1) as i32) as u32;
    desk_backend::CursorImage {
        width: size.width,
        height: size.height,
        hot_x: clamp(hotspot.0, size.width),
        hot_y: clamp(hotspot.1, size.height),
        bgra,
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut State,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            state.seat_pointer = caps.contains(wl_seat::Capability::Pointer);
        }
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, ()> for State {
    fn event(
        state: &mut State,
        _: &ExtImageCopyCaptureCursorSessionV1,
        event: ext_image_copy_capture_cursor_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use ext_image_copy_capture_cursor_session_v1::Event as E;
        match event {
            E::Enter => {
                state.cursor.inside = true;
                state.cursor.next_capture = None;
            }
            E::Leave => state.cursor.inside = false,
            // Takes effect with the next ready frame (protocol), which is
            // when service() reads it.
            E::Hotspot { x, y } => state.cursor.hotspot = (x, y),
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, CursorTag> for State {
    fn event(
        state: &mut State,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &CursorTag,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use ext_image_copy_capture_session_v1::Event as E;
        match event {
            E::BufferSize { width, height } => state.cursor.size = Some(Size { width, height }),
            E::ShmFormat {
                format: WEnum::Value(f),
            } => {
                // Only ARGB8888 carries the cursor's alpha; take anything else
                // only while nothing better was offered.
                if state.cursor.fmt.is_none() || f == wl_shm::Format::Argb8888 {
                    state.cursor.fmt = Some(f);
                }
            }
            E::Stopped => state.cursor.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, CursorTag> for State {
    fn event(
        state: &mut State,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &CursorTag,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use ext_image_copy_capture_frame_v1::Event as E;
        match event {
            E::Ready => state.cursor.ready = true,
            E::Failed { .. } => state.cursor.failed = true,
            _ => return,
        }
        // Done either way: destroying it re-arms the session for the next
        // capture (one live frame per session).
        frame.destroy();
        if state.cursor.frame.as_ref() == Some(frame) {
            state.cursor.frame = None;
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut State,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => {
                    state.shm =
                        Some(registry.bind::<wl_shm::WlShm, _, _>(name, version.min(1), qh, ()))
                }
                "wl_seat" => {
                    state.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ()))
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.capture_mgr = Some(registry.bind(name, version.min(1), qh, ()))
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.source_mgr = Some(registry.bind(name, version.min(1), qh, ()))
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.vptr_mgr = Some(registry.bind(name, version.min(2), qh, ()))
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.vkbd_mgr = Some(registry.bind(name, version.min(1), qh, ()))
                }
                "wl_output" => {
                    let out =
                        registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                    state.outputs.push(out);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut State,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let wl_output::Event::Name { name } = event {
            let id = proxy.id().protocol_id();
            state.output_names.push((id, name));
        }
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for State {
    fn event(
        _: &mut State,
        _: &ExtImageCopyCaptureManagerV1,
        _event: ext_image_copy_capture_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for State {
    fn event(
        _: &mut State,
        _: &ExtOutputImageCaptureSourceManagerV1,
        _event: ext_output_image_capture_source_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut State,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use ext_image_copy_capture_session_v1::Event as E;
        match event {
            E::BufferSize { width, height } => state.frame_size = Size { width, height },
            E::ShmFormat {
                format: WEnum::Value(f),
            } => {
                let preferred = matches!(f, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888);
                if state.shm_fmt.is_none() || preferred {
                    state.shm_fmt = Some(f);
                }
            }
            E::ShmFormat { .. } => {}
            E::Done => {}
            E::Stopped => state.failed = true,
            _ => {}
        }
    }

    event_created_child!(State, ExtImageCopyCaptureSessionV1, [
        0 => (ExtImageCopyCaptureFrameV1, ()),
    ]);
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut State,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use ext_image_copy_capture_frame_v1::Event as E;
        match event {
            E::Damage {
                x,
                y,
                width,
                height,
            } => state.damage.push(Rect {
                x,
                y,
                width: width as u32,
                height: height as u32,
            }),
            E::Ready => {
                // The frame is complete and its contents are in the shm pool.
                // destroy it now: the protocol forbids issuing another
                // capture on a session while a prior frame object is still
                // live ("duplicate frame"). The pump loop reads the pixels
                // from state.pool (not the frame object), so destroying the
                // frame here is safe and re-arms the session for the next
                // capture.
                state.buffer_ready = true;
                frame.destroy();
            }
            E::Failed { .. } => {
                state.failed = true;
                frame.destroy();
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Shm buffer pool (memfd + shm_open fallback + mmap), wrapped in wl_buffer
// ---------------------------------------------------------------------------

struct ShmBuffer {
    fd: i32,
    map: *mut u8,
    len: usize,
    buffer: wl_buffer::WlBuffer,
    #[allow(dead_code)]
    pool: wl_shm_pool::WlShmPool,
}

impl ShmBuffer {
    fn create(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        size: Size,
        fmt: wl_shm::Format,
    ) -> Result<Self> {
        // Checked size math (S2): a compositor-reported frame size feeds this,
        // and `width as i32 * 4` / `* height` overflow wraps into a small
        // backing file while the wl_buffer is created with the unwrapped
        // stride — a wild-write invitation.
        let stride_px = size
            .width
            .checked_mul(4)
            .filter(|s| *s <= i32::MAX as u32)
            .ok_or_else(|| {
                BackendError::Other(anyhow::anyhow!("frame width {} too large", size.width))
            })?;
        let len = (stride_px as usize)
            .checked_mul(size.height as usize)
            .filter(|l| *l <= i32::MAX as usize)
            .ok_or_else(|| {
                BackendError::Other(anyhow::anyhow!(
                    "frame {}x{} exceeds wl_shm pool limits",
                    size.width,
                    size.height
                ))
            })?;
        let stride = stride_px as i32;

        let fd = create_anonymous_file(len as i64)?;

        let map = unsafe {
            libc_mmap(
                std::ptr::null_mut(),
                len,
                0x1 | 0x2, // PROT_READ | PROT_WRITE
                0x01,      // MAP_SHARED
                fd,
                0,
            )
        };
        if map.is_null() || map as isize == -1 {
            let e = std::io::Error::last_os_error();
            let _ = unsafe { libc_close(fd) };
            return Err(BackendError::Other(anyhow::anyhow!(
                "mmap shm pool failed: {e}"
            )));
        }

        // Borrow the fd for the duration of the pool/buffer creation; we keep
        // ownership of `fd` in self for the map lifetime.
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let pool = shm.create_pool(bfd, len as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            size.width as i32,
            size.height as i32,
            stride,
            fmt,
            qh,
            (),
        );
        Ok(ShmBuffer {
            fd,
            map: map as *mut u8,
            len,
            buffer,
            pool,
        })
    }

    /// Copy the compositor-painted pixels out of the shared map.
    fn read_pixels(&self) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(self.map, self.len).to_vec() }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = libc_munmap(self.map as *mut std::ffi::c_void, self.len);
            let _ = libc_close(self.fd);
        }
    }
}

extern "C" {
    fn memfd_create(name: *const std::ffi::c_char, flags: u32) -> i32;
    fn shm_open(name: *const std::ffi::c_char, oflag: i32, mode: u32) -> i32;
    fn shm_unlink(name: *const std::ffi::c_char) -> i32;
    #[link_name = "ftruncate"]
    fn libc_ftruncate(fd: i32, len: i64) -> i32;
    #[link_name = "mmap"]
    fn libc_mmap(
        addr: *mut std::ffi::c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        off: i64,
    ) -> *mut std::ffi::c_void;
    #[link_name = "munmap"]
    fn libc_munmap(addr: *mut std::ffi::c_void, len: usize) -> i32;
    #[link_name = "close"]
    fn libc_close(fd: i32) -> i32;
}

const MFD_CLOEXEC: u32 = 0x0001;
const MFD_ALLOW_SEALING: u32 = 0x0002;
/// O_RDWR|O_CREAT|O_EXCL on Linux.
const O_RDWR_CREAT_EXCL: i32 = 0x2 | 0x40 | 0x80;

/// poll(2) POLLIN flag.
const POLLIN: i16 = 0x0001;

#[repr(C)]
struct LibcPollfd {
    fd: i32,
    events: i16,
    revents: i16,
}

extern "C" {
    #[link_name = "poll"]
    fn libc_poll_raw(fds: *mut LibcPollfd, nfds: u64, timeout: i32) -> i32;
}

/// Poll `fd` for readability for up to `timeout_ms` milliseconds. Returns the
/// reported `revents` (>= 0), or -1 on error.
fn libc_poll_read(fd: i32, timeout_ms: i32) -> i32 {
    let mut pfd = LibcPollfd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    let n = unsafe { libc_poll_raw(&mut pfd, 1, timeout_ms) };
    if n < 0 {
        -1
    } else {
        pfd.revents as i32
    }
}

/// Poll up to three fds; a fd of -1 is skipped (poll semantics). Returns the
/// three revents as i32; any may be 0 on timeout.
fn libc_poll_trio(wl_fd: i32, notify_fd: i32, wake_fd: i32, timeout_ms: i32) -> (i32, i32, i32) {
    let mut pfds = [
        LibcPollfd {
            fd: wl_fd,
            events: POLLIN,
            revents: 0,
        },
        LibcPollfd {
            fd: notify_fd,
            events: POLLIN,
            revents: 0,
        },
        LibcPollfd {
            fd: wake_fd,
            events: POLLIN,
            revents: 0,
        },
    ];
    let n = unsafe { libc_poll_raw(pfds.as_mut_ptr(), 3, timeout_ms) };
    if n < 0 {
        (0, 0, 0)
    } else {
        (
            pfds[0].revents as i32,
            pfds[1].revents as i32,
            pfds[2].revents as i32,
        )
    }
}

/// Drain an eventfd (reset its readiness). No-op on -1.
fn eventfd_drain(fd: i32) {
    if fd < 0 {
        return;
    }
    let mut n: u64 = 0;
    unsafe {
        libc::read(fd, (&mut n as *mut u64).cast::<libc::c_void>(), 8);
    }
}

fn create_anonymous_file(len: i64) -> Result<i32> {
    let name = std::ffi::CString::new("wayrdp-shm").unwrap();
    let fd = unsafe { memfd_create(name.as_ptr(), MFD_CLOEXEC | MFD_ALLOW_SEALING) };
    if fd >= 0 {
        let r = unsafe { libc_ftruncate(fd, len) };
        if r == 0 {
            return Ok(fd);
        }
        unsafe { libc_close(fd) };
    }
    // shm_open fallback (memfd may be blocked by seccomp).
    let path = std::ffi::CString::new(format!("/wayrdp-{}-{}", std::process::id(), len)).unwrap();
    let fd = unsafe { shm_open(path.as_ptr(), O_RDWR_CREAT_EXCL, 0o600) };
    if fd < 0 {
        return Err(BackendError::Other(anyhow::anyhow!(
            "no anonymous shm fd: memfd and shm_open both failed"
        )));
    }
    unsafe { shm_unlink(path.as_ptr()) };
    let r = unsafe { libc_ftruncate(fd, len) };
    if r != 0 {
        unsafe { libc_close(fd) };
        return Err(BackendError::Other(anyhow::anyhow!(
            "shm_open ftruncate failed"
        )));
    }
    Ok(fd)
}

macro_rules! noop_dispatch {
    ($iface:ty) => {
        impl Dispatch<$iface, ()> for State {
            fn event(
                _: &mut State,
                _: &$iface,
                _event: <$iface as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<State>,
            ) {
            }
        }
    };
}

noop_dispatch!(wl_pointer::WlPointer);
noop_dispatch!(wl_shm::WlShm);
noop_dispatch!(wl_shm_pool::WlShmPool);
noop_dispatch!(wl_buffer::WlBuffer);
noop_dispatch!(ZwlrVirtualPointerV1);
noop_dispatch!(ZwpVirtualKeyboardV1);
noop_dispatch!(ZwlrVirtualPointerManagerV1);
noop_dispatch!(ZwpVirtualKeyboardManagerV1);
noop_dispatch!(wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1);

// ---------------------------------------------------------------------------
// Virtual output management & input injection
// ---------------------------------------------------------------------------

/// Output refresh rate for the wayrdp headless output.
///
/// Hyprland only renders on damage, so a higher rate costs nothing at idle
/// but halves the render-tick wait during motion (16.7 ms @60 → 8.3 ms
/// @120). Default 60; override with WAYRDP_OUTPUT_HZ (clamped to 30..240;
/// invalid values fall back). Shared by create_headless_output and
/// repin_output_mode so a re-applied mode keeps the same rate instead of
/// silently dropping back to 60.
fn output_hz() -> u32 {
    std::env::var("WAYRDP_OUTPUT_HZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&h| (30..=240).contains(&h))
        .unwrap_or(60)
}

/// Create a `wayrdp-*` headless output at the requested size.
fn create_headless_output(size: Size, name: &str, scale: f64) {
    match hyprctl_run(&["output", "create", "headless", name]).map(|o| o.status) {
        Ok(s) if s.success() => {
            // Hyprland >= 0.50 rejects `keyword` for monitor (non-legacy
            // parser). `output create headless` auto-picks a default mode; pin
            // to the client size via Lua eval so non-native res has no bar.
            // Hyprland 0.56.2 Lua API (verified live): hl.monitor takes ONE table
            //   hl.monitor({ output = "<name>",        -- string, required
            //                mode = "<W>x<H>@<hz>",     -- string
            //                position = "0x0",          -- string (NOT a table)
            //                scale = 1 })               -- number
            // The old two-arg `hl.monitor.set_rule(name, {...position={x=0,y=0}})`
            // form no longer exists — hl.monitor is a function and set_rule is
            // not a field; calling it raised "attempt to index a function value
            // (field 'monitor')", so the headless output kept its auto default
            // mode and every RDP frame was dropped as a size mismatch.
            //
            //             // `scale` mirrors the host's desktopScaleFactor (HiDPI vmconnect):
            // Hyprland keeps the framebuffer at the mode size and renders
            // content larger, so text stays at the physical size the user's
            // display settings promise while the capture stays at client res.
            //
            // Refresh rate (e7): half the render-tick wait during motion;
            // WAYRDP_OUTPUT_HZ override documented on output_hz().
            let hz: u32 = output_hz();
            let lua = format!(
                "hl.monitor({{ output = \"{}\", mode = \"{}x{}@{}\", position = \"0x0\", scale = {} }})",
                name, size.width, size.height, hz, scale
            );
            let _ = hyprctl_run(&["eval", &lua]);
            migrate_workspaces_to_output(name);
        }
        other => eprintln!("[backend-wlroots] hyprctl output create headless -> {other:?}"),
    }
}

/// Move the user's populated workspaces onto our fresh headless output and
/// focus it — the Hyprland analog of the KWin LayoutGuard approach
/// (which disables the physical output so KWin migrates the whole desktop
/// onto the captured virtual one). Hyprland has no "disable output" IPC, but
/// achieves the same visible state: `hl.dsp.workspace.move` relocates each
/// populated workspace (with its windows) onto `name`; the now-empty
/// workspaces left on Virtual-1 auto-disappear (dynamic workspace GC), so
/// the physical output goes inert and everything sits on the captured output,
/// matching the KWin layout-guard semantics.
///
/// Done in ONE `hyprctl eval` batch. Focused through `hl.dsp.focus` so the
/// session opens on the user's windows instead of the output's fresh default
/// workspace. Failures are non-fatal: an unmigrated wayrdp output behaves
/// exactly like pre-migration wayrdp (empty default workspace).
fn migrate_workspaces_to_output(name: &str) {
    // ws.monitor is an HL.Monitor object (compare .name); ws.windows is a
    // COUNT (LuaWorkspace.cpp:116 getWindowCount), not a list.
    // Verbatim Hyprland 0.56 Lua surface (LuaBindingsDispatchers.cpp):
    // whole-workspace move is hl.dsp.workspace.move({workspace, monitor})
    // and monitor focus is hl.dsp.focus({monitor}) — both table-arg builders
    // returning a dispatcher OBJECT that must run through hl.dispatch(obj);
    // calling the object directly errors "use hl.dispatch(dispatcher)".
    // Verified live 2026-10-08 against Omarchy's running Hyprland 0.56.2.
    // Skip other wayrdp-N outputs: concurrent wayrdp sessions each own one,
    // and 35fa51d validated two simultaneous peers — without the guard a new
    // connect would yank every window out of the live sibling session.
    let lua = format!(
        r#"local me = "{name}"
for _, ws in ipairs(hl.get_workspaces()) do
  if ws.monitor and ws.monitor.name ~= me
     and not ws.monitor.name:find("^wayrdp%-")
     and ws.windows > 0 then
    pcall(hl.dispatch, hl.dsp.workspace.move({{ workspace = ws.id, monitor = me }}))
  end
end
hl.dispatch(hl.dsp.focus({{ monitor = me }}))"#
    );
    match hyprctl_run(&["eval", &lua]) {
        Ok(o) if o.status.success() => {
            eprintln!("[backend-wlroots] workspaces migrated onto {name}");
        }
        other => {
            eprintln!("[backend-wlroots] workspace migration onto {name} -> {other:?} (non-fatal)")
        }
    }
}

fn destroy_headless_output(name: &str) {
    let _ = hyprctl_run(&["output", "remove", name]);
}

/// Re-read the compositor's output layout (via `hyprctl monitors -j`) and
/// store the overall extent plus our output's origin for pointer mapping.
/// Retries briefly: right after `output create headless`/mode pin, the new
/// output may not be listed yet (compositor applies it asynchronously).
fn sync_layout(state: &mut State) {
    for _ in 0..10 {
        if let Some((extent, origin)) = query_layout(&state.output_name) {
            state.layout_extent = extent;
            state.layout_origin = origin;
            eprintln!(
                "[backend-wlroots] layout extent {}x{}, output origin {}x{}",
                extent.0, extent.1, origin.0, origin.1
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    state.layout_extent = (0, 0);
    state.layout_origin = (0, 0);
    eprintln!("[backend-wlroots] layout query failed; pointer extent falls back to frame size");
}

/// `hyprctl monitors -j` as a parsed array (empty when hyprctl/json fails,
/// which callers treat as "layout unknown yet").
fn monitors() -> Vec<serde_json::Value> {
    let out = match hyprctl_run(&["monitors", "-j"]) {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    serde_json::from_slice(&out.stdout)
        .ok()
        .and_then(|v: serde_json::Value| v.as_array().cloned())
        .unwrap_or_default()
}

/// True while `name` is an enabled entry in `hyprctl monitors`.
fn output_listed(name: &str) -> bool {
    monitors().iter().any(|m| {
        m.get("disabled").and_then(|d| d.as_bool()) != Some(true)
            && m.get("name").and_then(|n| n.as_str()) == Some(name)
    })
}

/// Parse `hyprctl monitors -j` into (layout extent, our output's origin).
/// Logical output size is pixel size / scale; the layout bounding box spans
/// every enabled output. None while our output isn't listed yet.
fn query_layout(own_name: &str) -> Option<((u32, u32), (i32, i32))> {
    let mons = monitors();
    if mons.is_empty() {
        return None;
    }
    let mut right = 0i64;
    let mut bottom = 0i64;
    let mut origin = None;
    for m in &mons {
        if m.get("disabled").and_then(|d| d.as_bool()) == Some(true) {
            continue;
        }
        let scale = m.get("scale").and_then(|s| s.as_f64())?.max(0.01);
        let w = (m.get("width")?.as_i64()? as f64 / scale).round() as i64;
        let h = (m.get("height")?.as_i64()? as f64 / scale).round() as i64;
        let x = m.get("x")?.as_i64()?;
        let y = m.get("y")?.as_i64()?;
        right = right.max(x + w);
        bottom = bottom.max(y + h);
        if m.get("name")?.as_str()? == own_name {
            origin = Some((x as i32, y as i32));
        }
    }
    if right <= 0 || bottom <= 0 {
        return None;
    }
    origin.map(|o| ((right as u32, bottom as u32), o))
}

/// Bind a fresh wl_output for our own headless output and create a new
/// capture source + session on it. Used at startup and after every resize:
/// resizing recreates the output, which invalidates the old wl_output and
/// therefore the old capture session (capturing on it throws the compositor
/// protocol error "invalid output resource"). `output` is re-bound on every
/// call; the old session/source are destroyed if present.
fn bind_capture(state: &mut State, queue: &mut EventQueue<State>) -> Result<()> {
    let qh = queue.handle();
    // The cursor session hangs off the source being replaced.
    state.cursor.release_session();
    if let Some(s) = state.session.take() {
        s.destroy();
    }
    if let Some(s) = state.source.take() {
        s.destroy();
    }
    // Give the compositor a chance to deliver/remove globals, then bind the
    // output currently carrying our name. Exact match only (S5): any
    // fallback — another session's wayrdp-* output, a HEADLESS output, or
    // the last enumerated output — can capture *someone else's desktop*,
    // which is both a confidentiality breach and silently wrong.
    let _ = queue.roundtrip(state);
    let output = state
        .output_names
        .iter()
        .rev()
        .find(|(_, n)| *n == state.output_name)
        .and_then(|(id, _)| {
            state
                .outputs
                .iter()
                .find(|o| o.id().protocol_id() == *id)
                .cloned()
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "own output {} not found in the current wl_output list (not rebinding to another output)",
                state.output_name
            )
        })?;

    let source_mgr = state
        .source_mgr
        .clone()
        .ok_or_else(|| anyhow::anyhow!("ext_output_image_capture_source_manager_v1 unavailable"))?;
    let capture_mgr = state
        .capture_mgr
        .clone()
        .ok_or_else(|| anyhow::anyhow!("ext_image_copy_capture_manager_v1 unavailable"))?;

    let source = source_mgr.create_source(&output, &qh, ());
    // Client-drawn cursor: leave the cursor out of the frames only when its
    // own session exists, so a failed bind degrades to the painted cursor
    // rather than to no cursor at all.
    let client_cursor = state.cursor.bind(
        state.seat.as_ref(),
        state.seat_pointer,
        &capture_mgr,
        &source,
        &qh,
    );
    let options = if client_cursor {
        ext_image_copy_capture_manager_v1::Options::empty()
    } else {
        ext_image_copy_capture_manager_v1::Options::PaintCursors
    };
    state.session = Some(capture_mgr.create_session(&source, options, &qh, ()));
    state.source = Some(source);
    state.failed = false;
    state.buffer_ready = false;
    let _ = queue.roundtrip(state);
    let _ = queue.roundtrip(state); // BufferSize / ShmFormat / Done
    Ok(())
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
fn rdp_scancode_to_evdev(scancode: u32) -> u32 {
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
fn rdp_scancode_to_evdev(scancode: u32) -> u32 {
    (scancode & !0x100).saturating_sub(8)
}

/// xkb modifier bit for an evdev keycode, if it is a modifier key.
/// Values match wl_keyboard/xkb mask bits (Shift=1<<0 .. Logo=1<<6).
fn evdev_mod_bit(key: u32) -> Option<u32> {
    Some(match key {
        42 | 54 => 1,    // KEY_LEFTSHIFT | KEY_RIGHTSHIFT
        29 | 97 => 4,    // KEY_LEFTCTRL | KEY_RIGHTCTRL
        56 | 100 => 8,   // KEY_LEFTALT | KEY_RIGHTALT
        125 | 126 => 64, // KEY_LEFTMETA | KEY_RIGHTMETA (Super)
        _ => return None,
    })
}

/// Send one key event, keeping the compositor-side modifier state in sync.
/// The zwp_virtual_keyboard_v1 `modifiers` state is how Hyprland evaluates
/// keybinds (Super+Enter & co); without it letters type but every chord is
/// a no-op. Emitted when a modifier key flips, before the key itself —
/// same ordering wtype uses.
///
/// Keystrokes are sensitive (passwords), so the trace lines stay behind
/// WAYRDP_DEBUG_INPUT; under the systemd unit stderr lands in the journal.
fn send_key_with_modifiers(
    state: &mut State,
    vkbd: &ZwpVirtualKeyboardV1,
    key: u32,
    pressed: bool,
) {
    if let Some(bit) = evdev_mod_bit(key) {
        let mask = if pressed {
            state.mod_mask | bit
        } else {
            state.mod_mask & !bit
        };
        if mask != state.mod_mask {
            state.mod_mask = mask;
            if std::env::var_os("WAYRDP_DEBUG_INPUT").is_some() {
                eprintln!("[input] vkbd modifiers: mask=0x{mask:x} (key={key} pressed={pressed})");
            }
            // Locked group: reflect the RDP lock-key sync instead of wiping
            // remote Caps/Num Lock on every Shift/Ctrl/Alt/Super change.
            vkbd.modifiers(mask, 0, vkbd_locked_bits(state.lock_bits), 0);
        }
    }
    if evdev_mod_bit(key).is_none() && std::env::var_os("WAYRDP_DEBUG_INPUT").is_some() {
        eprintln!(
            "[input] vkbd key: key={key} pressed={pressed} mod_mask=0x{:x}",
            state.mod_mask
        );
    }
    vkbd.key(0, key, u32::from(pressed));
    // Sync events only arrive on client focus changes, so remote lock-key
    // presses would otherwise be forgotten the moment the next modifier
    // change re-sends the old locked group. Track them by flipping the
    // bit on press (release can't toggle state).
    if pressed && (key == 58 || key == 69) {
        // KEY_CAPSLOCK / KEY_NUMLOCK
        let bit = if key == 58 { 0x04 } else { 0x02 }; // TS_SYNC_CAPS_LOCK / _NUM_LOCK
        state.lock_bits ^= bit;
        if std::env::var_os("WAYRDP_DEBUG_INPUT").is_some() {
            eprintln!(
                "[input] lock key toggled remotely: bits=0x{:x}",
                state.lock_bits
            );
        }
        vkbd.modifiers(state.mod_mask, 0, vkbd_locked_bits(state.lock_bits), 0);
    }
}

/// `last_pos` is now unused; kept in the signature for callers. The F3
/// relative-delta synthesis was removed (2026-10-08 pointer-smoothness):
/// Hyprland applies `vptr.motion()` (relative) and `vptr.motion_absolute()`
/// *on top of each other* within one frame instead of treating them as two
/// alternatives, so injecting "delta from last_pos, then absolute" moved the
/// cursor to `absolute + delta` — i.e. ~double the intended displacement
/// ("pointer jumps around the screen, not fluent"). vmconnect ESM sends an
/// absolute-only stream (HasRelativeMouseEvent is deliberately off), so the
/// relative channel buys nothing here.
fn inject(state: &mut State, ev: &InputEvent, _last_pos: &mut Option<(f64, f64)>) {
    match ev {
        InputEvent::PointerMotion { x, y } => {
            if let Some(p) = &state.vptr {
                // Extent covers the whole output layout (see State::layout_*);
                // fall back to the frame extent only when the layout query
                // failed and ours is therefore assumed to be the sole output.
                let (ew, eh) = if state.layout_extent.0 > 0 {
                    state.layout_extent
                } else {
                    (state.frame_size.width, state.frame_size.height)
                };
                let lx = (state.layout_origin.0 + *x).max(0) as f64;
                let ly = (state.layout_origin.1 + *y).max(0) as f64;
                p.motion_absolute(0, lx as u32, ly as u32, ew, eh);
                p.frame();
                // Metadata cursor relay (half-built; shape side missing, so
                // rdp-core only installs a slot under WAYRDP_POINTER_METADATA).
                // IMPORTANT: publish SESSION coords (x, y), not layout coords —
                // PTR_POS_UPDATE is session-space; sending lx/ly would double
                // the layout origin for clients that honor the packet.
                if let Some(slot) = state.pointer_pos_slot.as_ref() {
                    let packed = ((*x as u64 & 0xffff_ffff) << 32) | (*y as u64 & 0xffff_ffff);
                    slot.store(packed, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        InputEvent::PointerButton { button, pressed } => {
            if let Some(p) = &state.vptr {
                let code = match button {
                    PointerButton::Left => 0x110,
                    PointerButton::Right => 0x111,
                    PointerButton::Middle => 0x112,
                    PointerButton::Back => 0x116,
                    PointerButton::Forward => 0x115,
                };
                let st = if *pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                p.button(0, code, st);
                p.frame();
            }
        }
        InputEvent::PointerAxis { dx, dy } => {
            if let Some(p) = &state.vptr {
                // F8: RDP rotation units are positive=wheel-up/left in
                // 1/120ths of a notch; Wayland axis is positive=down/right —
                // invert. Compositors scale axis values like libinput, where
                // one wheel notch is ~15 units, so an unscaled ±120 scrolls
                // ~8 notches per notch. Divide by 8 to land at one notch per
                // RDP notch; axis_discrete still carries the true step count
                // on v2, so discrete-step clients stay exact.
                let to_axis = |rot: i32| -(rot as f64) / 8.0;
                if *dy != 0 {
                    let steps = -dy / 120;
                    let v = to_axis(*dy);
                    if state.vptr_v2 && steps != 0 {
                        p.axis_discrete(0, wl_pointer::Axis::VerticalScroll, v, steps);
                    } else {
                        p.axis(0, wl_pointer::Axis::VerticalScroll, v);
                    }
                }
                if *dx != 0 {
                    let steps = -dx / 120;
                    let v = to_axis(*dx);
                    if state.vptr_v2 && steps != 0 {
                        p.axis_discrete(0, wl_pointer::Axis::HorizontalScroll, v, steps);
                    } else {
                        p.axis(0, wl_pointer::Axis::HorizontalScroll, v);
                    }
                }
                p.frame();
            }
        }
        InputEvent::Key { scancode, pressed } => {
            // F1: go through WinPR's VK tables instead of the linear
            // scancode-8 shortcut — that path drops KBDSPECIAL/KBDNUMPAD keys
            // and mis-maps extended keys. 0 = unmapped; leave it to the
            // compositor rather than emitting a bogus KEY_ESC.
            let key = rdp_scancode_to_evdev(*scancode);
            if key == 0 {
                if std::env::var_os("WAYRDP_DEBUG_INPUT").is_some() {
                    eprintln!("[input] unmapped RDP scancode {scancode:#x}");
                }
                return;
            }
            // Autorepeat filter: mstsc synthesizes press floods for held
            // modifiers; a repeat press of an already-down key must not
            // reach the compositor (see keys_down).
            if *pressed {
                if !state.keys_down.insert(key) {
                    return; // already down: autorepeat, swallow
                }
            } else {
                state.keys_down.remove(&key);
            }
            if !state.keymap_set {
                // Compositor requires a keymap before the first key; hold,
                // bounded so a keymap-less compositor can't grow memory
                // without limit (F13).
                if state.key_buffer.len() < 256 {
                    state.key_buffer.push((key, *pressed));
                }
                return;
            }
            if let Some(k) = state.vkbd.clone() {
                send_key_with_modifiers(state, &k, key, *pressed);
            }
        }
        InputEvent::Unicode(_) => {} // resolved by client keymap; phase 4
        InputEvent::LockKeys { lock_bits } => {
            if *lock_bits != state.lock_bits {
                state.lock_bits = *lock_bits;
                if let Some(k) = state.vkbd.clone() {
                    k.modifiers(state.mod_mask, 0, vkbd_locked_bits(state.lock_bits), 0);
                }
            }
        }
    }
}

/// RDP toggleFlags (TS_SYNC_* bits in `InputEvent::LockKeys`) → xkb
/// "locked" mask for `zwp_virtual_keyboard_v1.modifiers` (Mod2=1<<4,
/// Caps=1<<1 on the compiled evdev/us keymap; xkb maps Caps onto whatever
/// LED-capable modifier the keymap defines).
fn vkbd_locked_bits(sync_bits: u8) -> u32 {
    let mut locked = 0;
    if sync_bits & 0x02 != 0 {
        locked |= 1 << 4; // TS_SYNC_NUM_LOCK -> Mod2
    }
    if sync_bits & 0x04 != 0 {
        locked |= 1 << 1; // TS_SYNC_CAPS_LOCK -> Caps Lock
    }
    locked
}

/// Upload the xkb keymap to the virtual keyboard, then replay any keys that
/// arrived before the keymap was installed. wlroots/Hyprland treat a key
/// event on a keymap-less virtual keyboard as a protocol error, so keymap
/// must precede the first `key()` call.
fn ensure_keymap(state: &mut State) {
    if state.keymap_set {
        return;
    }
    let Some(vkbd) = state.vkbd.clone() else {
        return;
    };
    match write_keymap(&state.kbd_layout.clone(), &state.kbd_variant.clone()) {
        Ok((fd, len)) => {
            let bfd = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
            // XKB keymap format = 1 (wl_keyboard.keymap_format.xkb_v1).
            vkbd.keymap(1, bfd, len);
            state.keymap_set = true;
            let drained: Vec<(u32, bool)> = std::mem::take(&mut state.key_buffer);
            for (key, pressed) in drained {
                send_key_with_modifiers(state, &vkbd, key, pressed);
            }
        }
        Err(e) => eprintln!("[backend-wlroots] keymap upload failed: {e}"),
    }
}

/// Map the client's RDP KeyboardLayout (LCID of the ACTIVE input language,
/// sent in TS_UD_CS_CORE) to an xkb (layout, variant) pair. Without this the
/// virtual keyboard always uses `us`, so non-US guests produce wrong glyphs
/// for every key whose position differs from en-US (the single biggest
/// perceived-keyboard bug for non-US users).
///
/// Exact-match first: layout-variant LCIDs (high word non-zero, e.g.
/// 0x00020409 US-International) carry the variant. Fallback: try the low 16
/// bits (FreeRDP-based clients and language packs can set the HIWORD to an
/// instance value), then give up with `us`. Unknowns print the LCID so gaps
/// get reported with an actionable number.
fn lcid_to_xkb(lcid: u32) -> (&'static str, &'static str) {
    let pair = match lcid {
        0x0409 => ("us", ""),      // English (US)
        0x0809 => ("gb", ""),      // English (UK)
        0x0C09 => ("au", ""),      // English (Australia)
        0x1409 => ("nz", ""),      // English (New Zealand)
        0x1009 => ("ca", "eng"),   // English (Canada)
        0x1809 => ("ie", ""),      // English (Ireland)
        0x20409 => ("us", "intl"), // US-International
        0x0413 => ("nl", ""),      // Dutch (Netherlands)
        0x0813 => ("be", ""),      // Dutch (Belgium, Belgian)
        0x0407 => ("de", ""),      // German (Germany)
        0x0807 => ("ch", "de"),    // German (Switzerland)
        0x0C07 => ("at", ""),      // German (Austria)
        0x040C => ("fr", ""),      // French (France)
        0x080C => ("be", ""),      // French (Belgium)
        0x0C0C => ("ca", ""),      // French (Canada)
        0x100C => ("ch", "fr"),    // French (Switzerland)
        0x0410 => ("it", ""),      // Italian
        0x040A => ("es", ""),      // Spanish (Spain)
        0x0816 => ("pt", ""),      // Portuguese (Portugal)
        0x0416 => ("br", ""),      // Portuguese (Brazil)
        0x041D => ("se", ""),      // Swedish
        0x040B => ("fi", ""),      // Finnish
        0x0414 => ("no", ""),      // Norwegian
        0x0406 => ("dk", ""),      // Danish
        0x040F => ("is", ""),      // Icelandic
        0x0415 => ("pl", ""),      // Polish
        0x0405 => ("cz", ""),      // Czech
        0x041B => ("sk", ""),      // Slovak
        0x040E => ("hu", ""),      // Hungarian
        0x0419 => ("ru", ""),      // Russian
        0x0411 => ("jp", ""),      // Japanese
        0x0412 => ("kr", ""),      // Korean
        _ => {
            // Language-only retry for layout-instance LCIDs (HIWORD != 0
            // forms like 0xF0xx0409 some clients emit).
            let low = lcid & 0xFFFF;
            if low != lcid {
                return lcid_to_xkb(low);
            }
            eprintln!(
                "[backend-wlroots] unknown client KB layout LCID 0x{lcid:x}; \
                 falling back to us (report this LCID to extend the map)"
            );
            ("us", "")
        }
    };
    pair
}

/// Compile the client's evdev/pc105 keymap via xkbcommon and write its
/// text form to a memfd, returning (fd, byte length).
///
/// We inject evdev keycodes (RDP scancode − 8), so the virtual keyboard's
/// keymap MUST map those keycodes to keysyms — Hyprland/wlroots resolves
/// keysyms through this keymap when forwarding to clients. The previous
/// hand-rolled keymap had an empty `xkb_symbols` block: xkbcomp errored on
/// it in Hyprland ("Key <LFSH> added to map for multiple modifiers"), and
/// even the keys that survived compiled to nothing, so every injected key
/// resolved to NoSymbol (observed: typing does nothing). Compiling the
/// default keymap gives us exactly the evdev naming/symbols the compositor
/// itself uses. `layout`/`variant` come from the client's advertised RDP
/// KeyboardLayout via `lcid_to_xkb`, so keys produce the glyphs the user's
/// keycaps promise ("us" when the client didn't say).
fn write_keymap(layout: &str, variant: &str) -> std::io::Result<(std::os::fd::OwnedFd, u32)> {
    use std::io::Write;
    use std::os::fd::FromRawFd;

    let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    // xkbcommon 0.8 takes RMLVO as &str (empty = system default). Pin evdev
    // rules / pc105 model: our injected keycodes are evdev codes (RDP
    // scancode − 8) and these are exactly the naming/symbols the compositor
    // itself compiles.
    let keymap = xkbcommon::xkb::Keymap::new_from_names(
        &ctx,
        "evdev",
        "pc105",
        layout,
        variant,
        None,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "xkbcommon refused default keymap",
        )
    })?;
    let text = keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1);

    // F13: CLOEXEC — the fd must not leak into hyprctl/helper children.
    let fd = unsafe { memfd_create(c"wayrdp-keymap".as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(text.as_bytes())?;
    Ok((f.into(), text.len() as u32))
}

// ---------------------------------------------------------------------------
// The pump loop
// ---------------------------------------------------------------------------

fn repin_output_mode(name: &str, size: Size, scale: f64, hz: u32) {
    // Re-assert the mode pin until `hyprctl monitors` reports the output at
    // the requested size. The pin inside create_headless_output races
    // Hyprland's asynchronous post-create default-mode assignment and loses
    // (observed live: eval returns ok yet the output stays 1920x1080 against
    // a negotiated 1920x1200 session). A repeat after the output has settled
    // sticks (the race is only at creation time; manual repins converge).
    // Returns with a log if it never converges — callers proceed; the extent
    // skew degrades pointer mapping, not video.
    for _attempt in 0..20 {
        if output_size_matches(name, size) {
            return;
        }
        let lua = format!(
            "hl.monitor({{ output = \"{name}\", mode = \"{}x{}@{hz}\", position = \"0x0\", scale = {scale} }})",
            size.width, size.height
        );
        let _ = hyprctl_run(&["eval", &lua]);
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    if !output_size_matches(name, size) {
        eprintln!(
            "[backend-wlroots] output {name} never reached {}x{}; pointer extent may be stale",
            size.width, size.height
        );
    }
}

/// True when `hyprctl monitors` lists `name` with exactly this logical size.
fn output_size_matches(name: &str, size: Size) -> bool {
    monitors().iter().any(|m| {
        m.get("disabled").and_then(|d| d.as_bool()) != Some(true)
            && m.get("name").and_then(|n| n.as_str()) == Some(name)
            && m.get("width").and_then(|w| w.as_u64()) == Some(size.width as u64)
            && m.get("height").and_then(|h| h.as_u64()) == Some(size.height as u64)
    })
}

/// Wait until the new headless output shows up in `hyprctl monitors`, plus a
/// short margin, before binding capture to it — otherwise binding a
/// `CImageCopyCaptureFrame` against a not-yet-committed output crashes
/// (`CImageCopyCaptureFrame::CImageCopyCaptureFrame`, observed on 0.56.2, taking
/// every client down).
fn settle_after_create(name: &str) {
    // First probe costs nothing when the output is already listed (common
    // case right after sync_layout has just seen it). Then poll at 50 ms;
    // the pump drains no input while we sleep here, so keep the worst case
    // well under the previous ~1.35 s.
    for attempt in 0..10 {
        if attempt != 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if output_listed(name) {
            std::thread::sleep(std::time::Duration::from_millis(150));
            return;
        }
    }
    eprintln!("[backend-wlroots] output {name} never appeared in hyprctl monitors");
}

/// Shared-memory reader for the hyprland-capture plugin.
///
/// The plugin hooks Hyprland's per-monitor render (RENDER_LAST_MOMENT) and
/// glReadPixels the already-painted output FB into a memfd, skipping the
/// ext-image-copy protocol's beginFullFakeRender dance. File layout:
///
/// ```text
/// [ FrameHeader (24 B) ] [ pixels: w*h*4 B, BGRA, row-major, top-down ]
/// ```
///
/// FrameHeader: magic u32 ("WCRC" = 0x57524343), width u32, height u32,
/// pad u32, seq u64 (24 bytes: the u64 member pulls struct alignment to 8).
/// seq is a seqlock — odd while the plugin is writing; we acquire-load it,
/// copy, and re-read to validate. Poll the seq; new even seq → copy pixels,
/// emit Frame.
///
/// Frame rate is bounded by Hyprland's own damage-driven rendering, which
/// on llvmpipe is ~25-50 fps under motion. The difference vs ext-image-copy:
/// we add ZERO compositor CPU per frame.
/// e8/v2: reader-written "released up to seq" words, banks 0/1 at
/// 168/176 (8-aligned; 160-167 hold active+pad, written by the plugin).
const REL0_OFFSET: usize = 168;
/// Client-drawn cursor handshake in the v2 header tail: the plugin stamps
/// its capability bits at 184 with every frame; the reader asks it to leave
/// Hyprland's software cursor out of this output's frames by writing
/// CURSOR_REQ_CLIENT at 188 (anything else = painted).
const CAPS_OFFSET: usize = 184;
const CURSOR_REQ_OFFSET: usize = 188;
const CAP_CLIENT_CURSOR: u32 = 1;
const CURSOR_REQ_CLIENT: u32 = 0x5253_4355; // "UCSR"

/// The plugin that wrote this header leaves the cursor out on request and
/// publishes its image. Only a v2 header has the caps word; an older plugin
/// leaves it zero.
fn plugin_supports_client_cursor(hdr_size: usize, header: &[u8]) -> bool {
    hdr_size == ShmCapture::HDR_SIZE_V2
        && header
            .get(CAPS_OFFSET..CAPS_OFFSET + 4)
            .is_some_and(|w| u32::from_ne_bytes(w.try_into().unwrap()) & CAP_CLIENT_CURSOR != 0)
}

/// Path of a capture/cursor segment shared with the plugin, under
/// `$XDG_RUNTIME_DIR` — tmpfs (so mmap gives the same shared-memory
/// semantics as /dev/shm) but 0700 and owned by the session user, so a
/// different local user cannot pre-create the name (the /dev/shm squat
/// denial-of-service). The plugin derives the identical path. None when no
/// runtime dir is set: there is no safe world-writable fallback, and a
/// Wayland session without one cannot host capture anyway.
fn shm_segment_path(kind: &str, output_name: &str) -> Option<std::path::PathBuf> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR")?;
    segment_path_in(&rt, kind, output_name)
}

/// [`shm_segment_path`] with the runtime dir as a parameter (test seam —
/// tests must not mutate the process env, which other parallel tests read).
fn segment_path_in(
    rt: &std::ffi::OsStr,
    kind: &str,
    output_name: &str,
) -> Option<std::path::PathBuf> {
    if rt.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(rt).join(format!("wayrdp-{kind}-{output_name}")))
}

/// Cursor image the capture plugin publishes for a client-drawn cursor
/// (`$XDG_RUNTIME_DIR/wayrdp-cursor-<output>`, see CursorHeader in
/// plugins/hyprland-capture/main.cpp): magic u32, version u32, seq u64
/// (seqlock), width u32, height u32, hot_x i32, hot_y i32, visible u32,
/// then ARGB8888 rows from byte 64.
struct PluginCursor {
    mmap: memmap2::Mmap,
    last_seq: u64,
}

impl PluginCursor {
    const MAGIC: u32 = 0x5743_5552;
    const HDR: usize = 64;
    const MAX: u32 = 256;

    fn path_for(output_name: &str) -> Option<std::path::PathBuf> {
        shm_segment_path("cursor", output_name)
    }

    fn open(path: &std::path::Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let f = std::fs::File::open(path).ok()?;
        let md = f.metadata().ok()?;
        if md.uid() != unsafe { libc::getuid() } {
            eprintln!(
                "[backend-wlroots] refusing cursor shm {path:?}: uid {} != ours",
                md.uid()
            );
            return None;
        }
        let mmap = unsafe { memmap2::MmapOptions::new().map(&f).ok()? };
        (mmap.len() >= Self::HDR).then_some(Self { mmap, last_seq: 0 })
    }

    /// The cursor state published since the last call, if any.
    fn poll(&mut self) -> Option<desk_backend::CursorUpdate> {
        use std::sync::atomic::{AtomicU64, Ordering};
        let read_seq = |m: &memmap2::Mmap| {
            // u64 at offset 8 of a page-aligned map: aligned, read atomically.
            let ptr = m[8..].as_ptr() as *const AtomicU64;
            unsafe { (*ptr).load(Ordering::Acquire) }
        };
        for _ in 0..3 {
            let before = read_seq(&self.mmap);
            if before == self.last_seq {
                return None;
            }
            if before & 1 == 0 {
                let update = parse_plugin_cursor(&self.mmap);
                if read_seq(&self.mmap) == before {
                    self.last_seq = before;
                    return update;
                }
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        None
    }
}

/// Decode a stable cursor shm snapshot. None = nothing usable (not written
/// yet, foreign layout, or out-of-bounds geometry).
fn parse_plugin_cursor(buf: &[u8]) -> Option<desk_backend::CursorUpdate> {
    let word = |o: usize| {
        buf.get(o..o + 4)
            .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
    };
    if word(0)? != PluginCursor::MAGIC || word(4)? != 1 {
        return None;
    }
    if word(32)? == 0 {
        return Some(desk_backend::CursorUpdate::Hidden);
    }
    let (w, h) = (word(16)?, word(20)?);
    if w == 0 || h == 0 || w > PluginCursor::MAX || h > PluginCursor::MAX {
        return None;
    }
    let px = buf.get(PluginCursor::HDR..PluginCursor::HDR + (w * h * 4) as usize)?;
    let hotspot = (word(24)? as i32, word(28)? as i32);
    let img = cursor_image_from_shm(
        px,
        Size {
            width: w,
            height: h,
        },
        wl_shm::Format::Argb8888,
        hotspot,
    );
    Some(desk_backend::CursorUpdate::Shape(Arc::new(img)))
}
struct ShmFile {
    mmap: memmap2::Mmap,
    dev: u64,
    ino: u64,
    ctime: i64,
    /// Kept for fstat() probes (cheap ctime staleness) and, in e8 double-
    /// buffered mode, for pwrite()ing slot-release words back into the
    /// header region so the plugin knows a bank is free for reuse.
    file: std::fs::File,
}
impl ShmFile {
    /// e8: publish-to-plugin "reader released bank `slot` having consumed
    /// frame `seq`". The word lives in the shm itself (offset 160+8*slot)
    /// so it survives reader restarts; pwrite keeps the mmap read-only.
    /// ~1µs syscall per dropped frame view; only used in double-buffered
    /// mode (the v2 plugin never reuses a bank without this handshake).
    fn release(&self, slot: usize, seq: u64) {
        if slot < 2 {
            use std::os::unix::io::AsRawFd;
            let bytes = seq.to_ne_bytes();
            unsafe {
                libc::pwrite(
                    self.file.as_raw_fd(),
                    bytes.as_ptr() as *const libc::c_void,
                    8,
                    (REL0_OFFSET + slot * 8) as libc::off_t,
                );
            }
        }
    }

    /// Ask the plugin to leave the cursor out of this output's frames (the
    /// client draws it) or to paint it again.
    fn request_client_cursor(&self, on: bool) {
        use std::os::unix::io::AsRawFd;
        let word = if on { CURSOR_REQ_CLIENT } else { 0 }.to_ne_bytes();
        unsafe {
            libc::pwrite(
                self.file.as_raw_fd(),
                word.as_ptr() as *const libc::c_void,
                4,
                CURSOR_REQ_OFFSET as libc::off_t,
            );
        }
    }
}

struct ShmCapture {
    file: Arc<ShmFile>,
    width: u32,
    height: u32,
    last_seq: u64,
    /// Header size/pixel offset for THIS file: 24 for a legacy plugin, 160
    /// for the v1 damage-rect extension (e2a). Detected at open by version
    /// field + file size; never re-read (the file can't change layout
    /// without being recreated, which the staleness probe catches).
    hdr_size: usize,
    /// e8: file holds TWO pixel banks when a v1 plugin sized it
    /// hdr + 2*px. The active bank for the *last published* frame is in the
    /// header at ACTIVE_OFFSET (0|1). Legacy/v0 files expose one bank.
    double_buffered: bool,
}

impl ShmCapture {
    const MAGIC: u32 = 0x5752_4343;
    // C++ FrameHeader legacy prefix: magic u32 (0-4), width u32 (4-8),
    // height u32 (8-12), pad u32 (12-16), seq u64 (16-24). Legacy pixels
    // start at byte 24; the v1 extension adds version u32 (24-28),
    // rect_count u32 (28-32) and 8 FrameRect u32x4 (32-160).
    const SEQ_OFFSET: usize = 16;
    const HDR_SIZE_LEGACY: usize = 24;
    const VERSION_OFFSET: usize = 24;
    const RECT_COUNT_OFFSET: usize = 28;
    const HDR_SIZE_V1: usize = 160;
    /// e8: v2 header (256 bytes). Bytes 0-159 = v1 layout VERBATIM (the
    /// 8 damage rects fill 32-159 — nothing free there); @160 = active
    /// bank selector u32 (0|1, plugin-written in the publish seqlock),
    /// @164 pad to 8-byte alignment, @168/@176 = per-bank "released up to
    /// seq" words written back by the reader; 184-255 reserved zero.
    /// Pixel banks start at 256.
    const HDR_SIZE_V2: usize = 256;
    const ACTIVE_OFFSET: usize = 160;

    /// Per-session segment path under $XDG_RUNTIME_DIR, keyed by this
    /// session's headless output name (see shm_segment_path). Two
    /// concurrent peers create distinct wayrdp-N outputs; a shared path
    /// makes the second (different-sized) peer read the first buffer and go
    /// black (observed with mstsc 1366x768 over a live 1920x1200 session).
    fn path_for(output_name: &str) -> Option<std::path::PathBuf> {
        shm_segment_path("capture", output_name)
    }

    /// Open and mmap the shm; None if it doesn't exist yet or fails the
    /// sanity gates. The plugin creates the file with 0600, fstats-checked
    /// uid ownership on its side; we additionally refuse anything not owned
    /// by us (a same-user pre-creation attack stays theoretical, but the
    /// check is free).
    fn open(path: &std::path::Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        // read+write: writing is only used for the e8 per-bank release
        // words (pwrite into the header region); the mmap itself stays
        // read-only.
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .ok()?;
        let md = f.metadata().ok()?;
        if md.uid() != unsafe { libc::getuid() } {
            eprintln!(
                "[backend-wlroots] refusing shm {path:?}: uid {} != ours",
                md.uid()
            );
            return None;
        }
        let mmap = unsafe { memmap2::MmapOptions::new().map(&f).ok()? };
        if mmap.len() < Self::HDR_SIZE_LEGACY {
            return None;
        }
        let (magic, w, h) = (
            u32::from_ne_bytes(mmap[0..4].try_into().ok()?),
            u32::from_ne_bytes(mmap[4..8].try_into().ok()?),
            u32::from_ne_bytes(mmap[8..12].try_into().ok()?),
        );
        if magic != Self::MAGIC || w == 0 || h == 0 {
            return None;
        }
        let px = (w as usize) * (h as usize) * 4;
        // e2a: v1 extended header (damage rects) iff the version field says
        // so AND the file is big enough to actually hold it. The size check
        // is what makes the probe safe against a legacy file whose first
        // pixel bytes happen to read as `1` — a legacy file is exactly 24+px
        // bytes, far smaller than 160+px, so pixel data can never be
        // mistaken for the extension.
        let version = if mmap.len() >= Self::HDR_SIZE_V1 + px {
            u32::from_ne_bytes(
                mmap[Self::VERSION_OFFSET..Self::VERSION_OFFSET + 4]
                    .try_into()
                    .ok()?,
            )
        } else {
            0
        };
        // e2a: v1 extended header (damage rects); e8: v2 = v1 layout plus
        // active-bank selector + reader release handshake, pixel banks at
        // 256. The size check makes the probe safe against a legacy file
        // whose first pixel bytes happen to read as a version — legacy
        // files are exactly 24+px, far smaller than any extended layout.
        let hdr_size = if version == 2 && mmap.len() >= Self::HDR_SIZE_V2 + 2 * px {
            Self::HDR_SIZE_V2
        } else if version == 1 && mmap.len() >= Self::HDR_SIZE_V1 + px {
            Self::HDR_SIZE_V1
        } else if mmap.len() < Self::HDR_SIZE_LEGACY + px {
            return None; // too small even for a single bank
        } else {
            Self::HDR_SIZE_LEGACY
        };
        let mmap_len = mmap.len();
        // e8: double-buffered (zero-copy handout) only with the v2 header —
        // a v1 file can't carry the release handshake, and a legacy/v1
        // writer would race a slot view with its next full-surface write.
        let double_buffered = hdr_size == Self::HDR_SIZE_V2 && mmap_len >= hdr_size + 2 * px;
        let dev = md.dev();
        let ino = md.ino();
        let file = Arc::new(ShmFile {
            mmap,
            dev,
            ino,
            ctime: md.ctime(),
            file: f,
        });
        if double_buffered {
            // Attach stamp: this (new) ShmCapture holds no views yet, so
            // both banks are free — and any words a previous reader left
            // behind (degenerate 0s included) must not stall the plugin's
            // next publishes. A bank's word only counts down when our views
            // drop (release(seq)); those are always > seqs the plugin has
            // since overwritten, so the handshake re-syncs within 2 frames.
            file.release(0, u64::MAX);
            file.release(1, u64::MAX);
        }
        Some(Self {
            width: w,
            height: h,
            last_seq: 0,
            hdr_size,
            double_buffered,
            file,
        })
    }

    fn supports_client_cursor(&self) -> bool {
        plugin_supports_client_cursor(self.hdr_size, &self.file.mmap)
    }

    /// Read the v1 damage-rect extension from the (stable) header, with
    /// per-rect bounds sanity. None = no valid rects advertised; caller
    /// falls back to full-surface damage. Read after the stable-seq check
    /// like the stamp, so the values pair with the pixels just copied.
    fn read_damage(&self) -> Option<Vec<Rect>> {
        if self.hdr_size < Self::HDR_SIZE_V1 {
            return None;
        }
        parse_damage_rects(
            &self.file.mmap[Self::RECT_COUNT_OFFSET..Self::HDR_SIZE_V1],
            self.width,
            self.height,
        )
    }

    /// True when the shm file we have mapped is stale: the path now points
    /// at a different inode (plugin recreated it after a resize/restart) or
    /// vanished. The caller drops and reopens; without this we could sit on
    /// an unlinked, never-written file and freeze at the old geometry.
    ///
    /// Layered for cost: `maybe_stale` (one fstat on the mapped fd, compares
    /// ctime) is the every-pass probe. Only when it fires do we do the full
    /// path check — the plugin only touches this file via the mmap, so a
    /// recreated file always ticks ctime and this misses nothing. Sub-second
    /// ctime granularity can false-positive within the same second the file
    /// was recreated; the full check disambiguates.
    fn maybe_stale(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        match self.file.file.metadata() {
            Ok(md) => md.ctime() != self.file.ctime,
            Err(_) => true,
        }
    }

    fn is_stale(&self, path: &std::path::Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(path) {
            Ok(md) => md.dev() != self.file.dev || md.ino() != self.file.ino,
            Err(_) => true,
        }
    }

    /// Poll for a new frame. Returns Some(Frame) if seq advanced since last
    /// call, else None. Skips ahead to the LATEST seq (intermediate frames
    /// are discarded — the encoder can't keep up with Hyprland's 200+ fps
    /// plugin and queueing stale pixels is the main source of perceived lag).
    ///
    /// Seqlock: the producer bumps seq to odd while writing pixels and to
    /// seq+1 (even) when done. If seq changes (or is odd) across our 9MB
    /// copy, the frame may be torn — retry with backoff, give up after a
    /// few attempts and return the torn frame never (better: wait for the
    /// next publish; callers tolerate a dropped frame, users see a torn
    /// one forever on a still screen).
    fn poll_frame(&mut self) -> Option<Frame> {
        use std::sync::atomic::{AtomicU64, Ordering};
        let read_seq = |m: &memmap2::Mmap| {
            // u64 at offset 16 is 8-aligned; read atomically.
            let ptr = m[Self::SEQ_OFFSET..].as_ptr() as *const AtomicU64;
            unsafe { (*ptr).load(Ordering::Acquire) }
        };
        let mut seq = read_seq(&self.file.mmap);
        if seq & 1 == 1 {
            // Producer mid-write; a fresh frame is ~milliseconds away.
            std::thread::sleep(std::time::Duration::from_millis(2));
            seq = read_seq(&self.file.mmap);
        }
        if seq & 1 == 1 || seq == self.last_seq {
            return None;
        }

        let (w, h) = (self.width as usize, self.height as usize);
        let px_start = self.hdr_size;
        let px_len = w * h * 4;
        // Hyprland renders its output FBO in display orientation (top row
        // first in glReadPixels result) — confirmed by probing live shm:
        // shm row h-1 holds the waybar top bar palette. So NO row flip.

        // e8: double-buffered file → the seqlock protects ONLY the v2
        // header (short reads torn ≤3 times). The bank written most
        // recently (hdr.active) is not being rewritten by the plugin while
        // we hold it — the plugin waits for our release(seq) pwrite before
        // reusing a bank, and drops its own frame after a bounded wait.
        if self.double_buffered {
            let mut active = u32::MAX;
            let mut stable_seq = seq;
            for _attempt in 0..4 {
                let seq_before = read_seq(&self.file.mmap);
                active = u32::from_ne_bytes(
                    self.file.mmap[Self::ACTIVE_OFFSET..Self::ACTIVE_OFFSET + 4]
                        .try_into()
                        .unwrap(),
                );
                let seq_after = read_seq(&self.file.mmap);
                if seq_after == seq_before && seq_after & 1 == 0 {
                    stable_seq = seq_after;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if active > 1 || stable_seq & 1 == 1 {
                // Corrupt header or still-unstable seq → skip this pass.
                return None;
            }
            self.last_seq = stable_seq;
            let stamp = u32::from_ne_bytes(self.file.mmap[12..16].try_into().unwrap());
            let damage = self.read_damage().unwrap_or_else(|| {
                vec![Rect {
                    x: 0,
                    y: 0,
                    width: self.width,
                    height: self.height,
                }]
            });
            let slot = active as usize;
            return Some(Frame {
                meta: FrameMeta {
                    size: Size {
                        width: self.width,
                        height: self.height,
                    },
                    stride: w * 4,
                    format: PixelFormat::Bgra32,
                    damage,
                    capture_stamp_ms: stamp,
                },
                pixels: std::sync::Arc::new(ShmSlotView {
                    file: Arc::clone(&self.file),
                    slot,
                    seq: stable_seq,
                    offset: px_start + slot * px_len,
                    len: px_len,
                }),
            });
        }

        // Single-buffered (legacy/v1 plugin): copy the current bank under
        // the seqlock retry. The pixel offset comes from the detected
        // header size, so a v1 (160-byte) file works unchanged.
        let row_bytes = w * 4;
        let mut out = Vec::with_capacity(px_len);
        let mut ok = false;
        for attempt in 0..4 {
            let seq_before = seq;
            for y in 0..h {
                let src_row = px_start + y * row_bytes;
                out.extend_from_slice(&self.file.mmap[src_row..src_row + row_bytes]);
            }
            let seq_after = read_seq(&self.file.mmap);
            if seq_after == seq_before && seq_after & 1 == 0 {
                ok = true;
                break;
            }
            out.clear();
            seq = seq_after;
            // Exponential-ish backoff; a render is a millisecons-scale stall.
            std::thread::sleep(std::time::Duration::from_millis(1 + attempt as u64 * 2));
        }
        if !ok {
            // Skip this frame entirely; keep last_seq so the NEXT stable
            // publish is consumed.
            return None;
        }
        self.last_seq = seq;

        // e12: publish timestamp (CLOCK_MONOTONIC ms, u32) the plugin stored
        // in the header's pad u32 at offset 12 inside the same seqlock
        // critical section as the pixels — read-after-stable-copy means the
        // value matches the pixels we just copied. Old plugins leave 0.
        let stamp = u32::from_ne_bytes(self.file.mmap[12..16].try_into().unwrap());

        // e2a: compositor damage rects from the v1 header when present and
        // sane; otherwise full-frame so downstream (tile diff, region
        // encode) can't skip unchanged bytes. Encode behavior unchanged —
        // rdp-core still encodes full-surface; the rects currently only
        // surface in the [pump-shm] stats to size up the planar threshold.
        let damage = self.read_damage().unwrap_or_else(|| {
            vec![Rect {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            }]
        });

        Some(Frame {
            meta: FrameMeta {
                size: Size {
                    width: self.width,
                    height: self.height,
                },
                stride: w * 4,
                format: PixelFormat::Bgra32,
                damage,
                capture_stamp_ms: stamp,
            },
            pixels: std::sync::Arc::new(out.into_boxed_slice()),
        })
    }
}

/// Parse the damage-rect payload of a v1 shm header (rect_count u32, then
/// `count` × {x,y,w,h} u32). None means "no usable rects": legacy header,
/// count 0 (plugin saw empty/oversized damage and intentionally signalled
/// full-surface) or ANY rect malformed — partial damage is worse than full,
/// so a single bad rect discards the whole set. u64 math so x+w can't wrap.
/// Zero-copy pixel view: wraps the shared ShmFile mmap and derefs to the
/// one active slot's pixel region. Active slot is negotiated per Frame; the
/// plugin never rewrites that slot's pixels until the peer releases it.
struct ShmSlotView {
    file: Arc<ShmFile>,
    slot: usize,
    seq: u64,
    offset: usize,
    len: usize,
}
impl std::ops::Deref for ShmSlotView {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // memmap2 ties deref lifetime to &self; the mmap outlives self
        // because `file` (Arc<ShmFile>) is a field. Safe: file alive ⇒ mmap alive.
        let mmap: &memmap2::Mmap = &self.file.mmap;
        &mmap[self.offset..self.offset + self.len]
    }
}
impl Drop for ShmSlotView {
    fn drop(&mut self) {
        // Handshake back to the plugin: bank `slot` consumed up to `seq`,
        // free for reuse. pwrite into the shm header, ~1µs.
        self.file.release(self.slot, self.seq);
    }
}

fn parse_damage_rects(buf: &[u8], width: u32, height: u32) -> Option<Vec<Rect>> {
    if buf.len() < 4 {
        return None;
    }
    let count = u32::from_ne_bytes(buf[0..4].try_into().ok()?) as usize;
    if count == 0 || count > 8 || buf.len() < 4 + count * 16 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = 4 + i * 16;
        let rd = |o: usize| u32::from_ne_bytes(buf[base + o..base + o + 4].try_into().unwrap());
        let (x, y, w, h) = (rd(0), rd(4), rd(8), rd(12));
        if w == 0 || h == 0 {
            return None;
        }
        if x as u64 + w as u64 > width as u64 || y as u64 + h as u64 > height as u64 {
            return None;
        }
        out.push(Rect {
            x: x as i32,
            y: y as i32,
            width: w,
            height: h,
        });
    }
    Some(out)
}

fn pump_loop(
    initial: Size,
    tx: FrameTx,
    resize_slot: Arc<Mutex<Option<Size>>>,
    input_rx: Receiver<desk_backend::StampedInput>,
    pointer_pos_slot: Option<desk_backend::PointerPositionSlot>,
    prefs: WlSessionPrefs,
    input_wake_fd: i32,
    frame_wake: Option<desk_backend::FrameWake>,
    cursor_slot: Option<desk_backend::PointerShapeSlot>,
) {
    eprintln!(
        "[backend-wlroots] pump start @ {}x{} scale={} kbd={}{}",
        initial.width,
        initial.height,
        prefs.output_scale,
        prefs.kbd_layout,
        if prefs.kbd_variant.is_empty() {
            String::new()
        } else {
            format!("({})", prefs.kbd_variant)
        }
    );
    // Serialize output lifecycle across overlapping sessions: the previous
    // pump tears its own output down before we touch anything. claim_pump
    // returns the lease itself — the flag is ours only while we hold this
    // guard, so cleanup and "we own the lifecycle" can't diverge.
    let lease = claim_pump();

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[backend-wlroots] Wayland connect failed (no Wayland session?): {e}");
            return;
        }
    };

    let mut event_queue = conn.new_event_queue::<State>();
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::default();
    // Client-drawn-cursor slot (Metadata mode, env-gated): if rdp-core wired
    // one in before start(), motion injects publish the newest session-space
    // position for rdp-core's PTR_POS_UPDATE relay, bypassing the frame
    // pipeline. None => cursor stays painted in the captured video.
    state.pointer_pos_slot = pointer_pos_slot;
    // Client-drawn cursor (WAYRDP_CLIENT_CURSOR): bind_capture creates a
    // cursor session next to the output session and leaves the cursor out
    // of the frames; the pump publishes its image into this slot.
    state.cursor.slot = cursor_slot;
    // Per-client keyboard/dpi presentation prefs from the session request:
    // XKB layout for the virtual keyboard keymap, compositor scale for the
    // headless output (HiDPI hosts keep text at physical size).
    state.kbd_layout = prefs.kbd_layout;
    state.kbd_variant = prefs.kbd_variant;
    state.output_scale = prefs.output_scale;

    // Discover globals, then outputs (names/modes arrive on a second roundtrip).
    if event_queue.roundtrip(&mut state).is_err() {
        eprintln!("[backend-wlroots] registry roundtrip failed");
        return;
    }
    let _ = event_queue.roundtrip(&mut state);

    // Capture-path selection. Default: ext-image-copy (Wayland-native).
    // WAYRDP_CAPTURE=shm-plugin: read frames from the hyprland-capture
    // plugin's shm, skipping the protocol entirely (the plugin's glReadPixels
    // at RENDER_LAST_MOMENT is ~1ms vs Hyprland's 40-60ms fake repaint).
    let use_shm_plugin = std::env::var("WAYRDP_CAPTURE")
        .map(|v| v == "shm-plugin")
        .unwrap_or(false);

    if !use_shm_plugin && (state.capture_mgr.is_none() || state.source_mgr.is_none()) {
        eprintln!("[backend-wlroots] ext-image-copy-capture unavailable; compositor lacks the staging protocol");
        return;
    }
    if use_shm_plugin {
        eprintln!("[backend-wlroots] capture mode: shm-plugin (wayrdp-capture Hyprland plugin)");
    }
    // We claim the lease before touching outputs; any session that already
    // holds it is mid-teardown of the same output we're about to walk over.
    // Lease is held to end of pump_loop; Drop clears PUMP_ACTIVE.
    match &lease {
        Some(_) => cleanup_stale_outputs(),
        None => {
            eprintln!("[backend-wlroots] couldn't take pump lease; skipping stale-output cleanup")
        }
    }
    let _lease = lease;
    state.output_name = next_output_name();
    eprintln!("[backend-wlroots] headless output {}", state.output_name);
    live_output_add(&state.output_name);
    create_headless_output(initial, &state.output_name, state.output_scale);
    // First sync is best-effort: the hl.monitor mode-pin inside
    // create_headless_output is applied asynchronously, so this read can
    // still see the output's default mode (1920x1080) instead of the
    // client size. settle_after_create + repin below wait out that
    // transition; the re-sync then sees the FINAL layout.
    // Observed live: extent stayed 2432x1080 against a 2432x1200 layout —
    // every injected absolute motion then landed at y*1200/1080 (11% past
    // the target, wedging at edges and fighting the client's cursor loop).
    sync_layout(&mut state);
    let _ = event_queue.roundtrip(&mut state);
    settle_after_create(&state.output_name);
    repin_output_mode(&state.output_name, initial, state.output_scale, output_hz());
    sync_layout(&mut state);

    if use_shm_plugin {
        // Enter the shm-only pump loop: no ext-image-copy session, no shm
        // pool for Wayland capture. Still drive Wayland input + monitor
        // lifecycle; pixel flow comes from the plugin's segment under
        // $XDG_RUNTIME_DIR (wayrdp-capture-<out>).
        pump_loop_shm(
            state,
            event_queue,
            initial,
            tx,
            resize_slot,
            input_rx,
            input_wake_fd,
            frame_wake,
        );
        return;
    }

    if bind_capture(&mut state, &mut event_queue).is_err() {
        eprintln!(
            "[backend-wlroots] no wl_output to capture for {}",
            state.output_name
        );
        let name = state.output_name.clone();
        live_output_remove(&name);
        destroy_headless_output(&name);
        return;
    }

    if let (Some(m), Some(seat)) = (&state.vptr_mgr, &state.seat) {
        state.vptr_v2 = m.version() >= 2;
        if !state.vptr_v2 {
            eprintln!(
                "[backend-wlroots] virtual-pointer v1; falling back to smooth axis for wheel (F8)"
            );
        }
        state.vptr = Some(m.create_virtual_pointer(Some(seat), &qh, ()));
    }
    if let (Some(m), Some(seat)) = (&state.vkbd_mgr, &state.seat) {
        state.vkbd = Some(m.create_virtual_keyboard(seat, &qh, ()));
    }
    // Install a keymap up front (flushing any keys that arrived early) so the
    // first real key event doesn't trip the compositor's keymap-first rule.
    ensure_keymap(&mut state);
    let _ = event_queue.roundtrip(&mut state);

    // Allocate the shm pool from the negotiated constraints.
    if state.frame_size.width == 0 {
        state.frame_size = initial;
    }
    let shm = match state.shm.as_ref() {
        Some(s) => s.clone(),
        None => {
            eprintln!("[backend-wlroots] wl_shm unavailable; compositor offers no shm readback");
            let name = state.output_name.clone();
            live_output_remove(&name);
            destroy_headless_output(&name);
            return;
        }
    };
    let fmt = state.shm_fmt.unwrap_or(wl_shm::Format::Argb8888);
    match ShmBuffer::create(&shm, &qh, state.frame_size, fmt) {
        Ok(p) => state.pool = Some(p),
        Err(e) => {
            eprintln!("[backend-wlroots] shm pool alloc failed: {e}");
            let name = state.output_name.clone();
            live_output_remove(&name);
            destroy_headless_output(&name);
            return;
        }
    }
    eprintln!(
        "[backend-wlroots] capture live on output @ {}x{} fmt {:?} (shm readback)",
        state.frame_size.width, state.frame_size.height, fmt
    );

    let mut session = state.session.clone().unwrap();
    let mut request_pending = false;
    let mut fail_attempts = 0u32;
    let mut _frames_sent = 0u64;
    // Per-frame phase timing (rolling 30-frame window) to find the chokepoint.
    // Phases: T_issue (capture req → Ready), T_dispatch (Ready → pixels sent).
    let mut t_issue_ms = std::collections::VecDeque::<u64>::with_capacity(30);
    let mut t_dispatch_ms = std::collections::VecDeque::<u64>::with_capacity(30);
    let mut frame_start: Option<std::time::Instant> = None;
    // (F3 removed: see inject() comment — absolute-only stream, the
    // synthesized relative delta was being applied on top of the absolute
    // warp and doubling cursor displacement.)
    macro_rules! inject_ev {
        ($ev:expr) => {{
            let mut _drop_last_pos: Option<(f64, f64)> = None;
            inject(&mut state, &($ev), &mut _drop_last_pos);
        }};
    }

    'main: loop {
        // Resize latest-wins (F7): the slot holds at most one request.
        let pending_resize = resize_slot.lock().unwrap_or_else(|p| p.into_inner()).take();

        let mut resized = false;

        if let Some(sz) = pending_resize {
            // No cursor frame may be in flight on the output being removed.
            state.cursor.release_session();
            destroy_headless_output(&state.output_name);
            create_headless_output(sz, &state.output_name, state.output_scale);
            sync_layout(&mut state);
            state.frame_size = sz;
            resized = true;
            request_pending = false;
        }

        if resized {
            // The output was recreated: the old capture session references a
            // wl_output that no longer exists ("invalid output resource").
            // Rebind output + source + session and realloc the shm pool.
            state.pool = None;
            settle_after_create(&state.output_name);
            // Re-assert the mode pin: the create-time pin races Hyprland's
            // async post-create default-mode assignment and can lose, leaving
            // the output at its realized default instead of the negotiated
            // size (pointer drift). converge on the client size here.
            repin_output_mode(
                &state.output_name,
                state.frame_size,
                state.output_scale,
                output_hz(),
            );
            // Post-settle re-sync (see startup note): the mode pin applied
            // during create_headless_output lands asynchronously, so the
            // pre-settle sync_layout above may hold a stale extent.
            sync_layout(&mut state);
            if bind_capture(&mut state, &mut event_queue).is_err() {
                eprintln!("[backend-wlroots] capture rebind after resize failed");
                break 'main;
            }
            session = state.session.clone().unwrap();
            match ShmBuffer::create(&shm, &qh, state.frame_size, fmt) {
                Ok(p) => state.pool = Some(p),
                Err(e) => {
                    eprintln!("[backend-wlroots] shm realloc after resize failed: {e}");
                    break 'main;
                }
            }
        }

        // Issue exactly one capture request at a time, awaiting its Ready.
        // Placed BEFORE the input drain: with the desktop idle the queue is
        // empty at the top of the loop, so draining there costs a whole
        // dispatch wait (up to ~100ms) before a motion that arrives during
        // the wait is folded into the next frame — the cursor then visibly
        // trails the picture ("drag lag"). Draining immediately before the
        // blocking read lets any late motion land in the very next frame.
        if !request_pending && !state.buffer_ready {
            if let Some(pool) = &state.pool {
                state.damage.clear();
                let frame = session.create_frame(&qh, ());
                frame.attach_buffer(&pool.buffer);
                frame.capture();
                let _ = event_queue.flush();
                request_pending = true;
                frame_start = Some(std::time::Instant::now());
            }
        }

        // Drain the input queue between dispatches, coalescing consecutive
        // PointerMotion events to the newest (F7) — the compositor only
        // needs the final cursor position of a burst.
        let mut coalesced_motion: Option<(i32, i32)> = None;
        while let Ok(sev) = input_rx.try_recv() {
            match sev.event {
                InputEvent::PointerMotion { x, y } => coalesced_motion = Some((x, y)),
                other => {
                    if let Some((x, y)) = coalesced_motion.take() {
                        inject_ev!(InputEvent::PointerMotion { x, y });
                    }
                    inject_ev!(other);
                }
            }
        }
        if let Some((x, y)) = coalesced_motion {
            inject_ev!(InputEvent::PointerMotion { x, y });
        }

        // Client-drawn cursor: publish a changed shape and wake the peer
        // mainloop so it reaches the client without waiting for a frame.
        if state.cursor.service(&shm, &qh) {
            if let Some(wake) = &frame_wake {
                wake();
            }
        }

        if state.buffer_ready {
            state.buffer_ready = false;
            request_pending = false;
            let t_ready = frame_start
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);
            let dispatch_start = std::time::Instant::now();
            if let Some(pool) = &state.pool {
                let pixels =
                    std::sync::Arc::new(pool.read_pixels().into_boxed_slice()) as FramePixels;
                // shm gives us ARGB8888 (memory order B,G,R,A on LE) or
                // XRGB8888 (B,G,R,X) — both are BGRA byte order on the wire,
                // matching PixelFormat::Bgra32 that rdp-core's encoder wants.
                let pix_fmt = PixelFormat::Bgra32;
                if tx
                    .send(Frame {
                        meta: FrameMeta {
                            size: state.frame_size,
                            stride: (state.frame_size.width as usize) * 4,
                            format: pix_fmt,
                            damage: std::mem::take(&mut state.damage),
                            // Stamp at capture completion: readback returned.
                            capture_stamp_ms: now_mono_ms_u32(),
                        },
                        pixels,
                    })
                    .is_err()
                {
                    break 'main; // encoder hung up -> peer disconnected
                }
                if let Some(wake) = &frame_wake {
                    wake();
                }
                _frames_sent += 1;
                fail_attempts = 0;
                let t_send = dispatch_start.elapsed().as_millis() as u64;
                if t_issue_ms.len() == 30 {
                    t_issue_ms.pop_front();
                }
                if t_dispatch_ms.len() == 30 {
                    t_dispatch_ms.pop_front();
                }
                t_issue_ms.push_back(t_ready);
                t_dispatch_ms.push_back(t_send);
                if _frames_sent % 60 == 0
                    && !t_issue_ms.is_empty()
                    && std::env::var_os("WAYRDP_DEBUG_CAPTURE").is_some()
                {
                    let avg_i: u64 =
                        t_issue_ms.iter().sum::<u64>() / t_issue_ms.len().max(1) as u64;
                    let avg_d: u64 =
                        t_dispatch_ms.iter().sum::<u64>() / t_dispatch_ms.len().max(1) as u64;
                    // Histogram capture time in 8ms buckets: how often does each bucket hit?
                    let mut buckets = [0u32; 16]; // 0-8, 8-16, ..., 120-128, 128+
                    for &t in &t_issue_ms {
                        let b = ((t / 8) as usize).min(15);
                        buckets[b] += 1;
                    }
                    let hist: Vec<String> = buckets
                        .iter()
                        .enumerate()
                        .filter(|(_, &c)| c > 0)
                        .map(|(i, &c)| format!("{}ms:{}", i * 8, c))
                        .collect();
                    eprintln!(
                        "[pump-timer] n={} cap_avg={}ms disp_avg={}ms fps_cap={:.1} hist=[{}]",
                        _frames_sent,
                        avg_i,
                        avg_d,
                        1000.0 / ((avg_i + avg_d).max(1) as f64),
                        hist.join(" ")
                    );
                }
            }
        }

        if state.failed {
            eprintln!("[backend-wlroots] capture session reported failure");
            // A capture session on a just-created headless output often fails
            // once (mode not yet committed compositor-side) while the exact
            // same bind on the settled output succeeds — every older session
            // ran capture successfully on its second attempt. Retry cheaply
            // (rebind source+session on the existing output) first, escalate
            // to a full output recreate on repeated failure, give up after 4.
            fail_attempts += 1;
            if fail_attempts > 4 {
                eprintln!("[backend-wlroots] capture failed repeatedly; giving up");
                break 'main;
            }
            state.failed = false;
            state.buffer_ready = false;
            request_pending = false;
            std::thread::sleep(std::time::Duration::from_millis(400));
            if fail_attempts >= 3 {
                eprintln!("[backend-wlroots] capture retry: recreating output");
                state.pool = None;
                state.cursor.release_session();
                live_output_remove(&state.output_name);
                destroy_headless_output(&state.output_name);
                state.output_name = next_output_name();
                eprintln!("[backend-wlroots] headless output {}", state.output_name);
                live_output_add(&state.output_name);
                create_headless_output(state.frame_size, &state.output_name, state.output_scale);
                sync_layout(&mut state);
            }
            settle_after_create(&state.output_name);
            // Re-pin the mode after settle (same create-time race as the shm
            // pump / startup paths): without it the recreated output can hold
            // the default mode and produce a size-mismatch capture retry loop.
            repin_output_mode(
                &state.output_name,
                state.frame_size,
                state.output_scale,
                output_hz(),
            );
            sync_layout(&mut state);
            if bind_capture(&mut state, &mut event_queue).is_err() {
                eprintln!("[backend-wlroots] capture retry rebind failed");
                continue 'main;
            }
            session = state.session.clone().unwrap();
            if state.pool.is_none() {
                match ShmBuffer::create(&shm, &qh, state.frame_size, fmt) {
                    Ok(p) => state.pool = Some(p),
                    Err(e) => {
                        eprintln!("[backend-wlroots] shm realloc during retry failed: {e}");
                        break 'main;
                    }
                }
            }
            continue 'main;
        }

        // Disconnect/watchdog: if the session was dropped, its cmd_tx is gone
        // (set to None in Drop). try_recv on a hung-up Receiver returns
        // Disconnected; treat that as shutdown so an idle desktop doesn't
        // block forever in dispatch and leak the headless output. Inputs
        // arriving here after the drain window are still injected; only Drop
        // (which sends no commands) can close the channel, so nothing real
        // is ever discarded.
        match input_rx.try_recv() {
            Err(TryRecvError::Disconnected) => {
                // Teardown timeline marker: pump saw the peer drop the channel.
                eprintln!(
                    "[backend-wlroots] teardown {}: pump cmd channel hung up",
                    state.output_name
                );
                state.shutdown = true;
            }
            Ok(sev) => inject_ev!(sev.event),
            Err(TryRecvError::Empty) => {}
        }
        if state.shutdown {
            break 'main;
        }

        // Bounded wait instead of indefinite blocking_dispatch: poll the
        // wayland fd for up to 100 ms while idle so the loop wakes even when
        // the compositor produces no events (idle desktop, client
        // disconnected), letting the shutdown check above run. While a
        // capture request is in flight or a captured buffer is waiting to be
        // dispatched to the encoder, drop the timeout hard: we expect the
        // compositor's Ready event imminently and stalling here would cap
        // effective FPS. prepare_read takes &self, so this borrows event_queue
        // immutably until the guard is dropped, then dispatch_pending takes
        // &mut.
        let _ = event_queue.flush();
        if let Some(guard) = event_queue.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            let timeout_ms = if request_pending || state.buffer_ready {
                8
            } else {
                100
            };
            // Wake for a due cursor capture rather than a full idle period.
            let timeout_ms = state
                .cursor
                .wait_ms()
                .map_or(timeout_ms, |w| timeout_ms.min(w.max(1)));
            let revents = libc_poll_read(fd, timeout_ms);
            if revents >= 0 && (revents as i16) & POLLIN != 0 {
                // read() consumes the guard.
                if let Err(e) = guard.read() {
                    eprintln!("[backend-wlroots] wayland read error: {e:?}");
                    break 'main;
                }
            } else {
                // No events read; drop the guard so it doesn't re-read.
                drop(guard);
            }
        }
        if let Err(e) = event_queue.dispatch_pending(&mut state) {
            eprintln!("[backend-wlroots] wayland dispatch error: {e}");
            break 'main;
        }
    }

    // F9: release our virtual input devices before tearing the output down.
    // ZwlrVirtualPointerV1 is a destructor request (protocol); the keyboard
    // has no such request — dropping the proxy just unmaps the client object,
    // which is all we can do short of disconnecting.
    if let Some(p) = state.vptr.take() {
        p.destroy();
    }
    drop(state.vkbd.take());
    state.cursor.release_session();
    if let Some(p) = state.cursor.pointer.take() {
        p.release();
    }
    drop(state.session.take());
    drop(state.source.take());
    let _ = event_queue.flush();
    live_output_remove(&state.output_name);
    destroy_headless_output(&state.output_name);
    eprintln!(
        "[backend-wlroots] teardown {}: output remove returned",
        state.output_name
    );
    eprintln!("[backend-wlroots] pump exit");
}

/// Unix datagram socket the pump binds at
/// `$XDG_RUNTIME_DIR/wayrdp-capture-<output>.sock`: the plugin sendto()s one
/// datagram per frame right after publishing its seq, so polling this socket
/// wakes us on the frame instead of the 8 ms seq-poll ceiling. Old plugins
/// don't send notifies — bind failure or silence just falls back to the
/// timeout path, keeping both directions version-compatible.
struct FrameNotify {
    fd: std::os::unix::io::RawFd,
    path: std::path::PathBuf,
}

impl FrameNotify {
    fn bind(output_name: &str) -> Option<Self> {
        use std::os::unix::ffi::OsStrExt;
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        let path = base.join(format!("wayrdp-capture-{output_name}.sock"));
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return None;
        }
        // Non-blocking so draining after POLLIN can't stall the pump.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        // Remove a stale socket from a previous run: it is dead on arrival
        // (nobody listens), and bind() on an existing path fails with EADDRINUSE.
        let _ = std::fs::remove_file(&path);
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() + 1 > addr.sun_path.len() {
            unsafe {
                libc::close(fd);
            }
            return None;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                addr.sun_path.as_mut_ptr() as *mut u8,
                bytes.len(),
            );
        }
        let rc = unsafe {
            libc::bind(
                fd,
                (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            eprintln!(
                "[backend-wlroots] notify socket bind {path:?} failed: {err}; using poll fallback"
            );
            unsafe {
                libc::close(fd);
            }
            return None;
        }
        Some(Self { fd, path })
    }

    /// Discard every queued datagram. Content is irrelevant (plugin sends its
    /// seq); a wake means "poll the shm now".
    fn drain(&self) {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe {
                libc::recv(
                    self.fd,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                break;
            }
        }
    }
}

impl Drop for FrameNotify {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Pump loop for WAYRDP_CAPTURE=shm-plugin: replaces ext-image-copy with
/// direct reads from the hyprland-capture plugin's shm. Still owns the
/// wayrdp-N headless output lifecycle and virtual input.
fn pump_loop_shm(
    mut state: State,
    event_queue: EventQueue<State>,
    initial: Size,
    tx: FrameTx,
    resize_slot: Arc<Mutex<Option<Size>>>,
    input_rx: Receiver<desk_backend::StampedInput>,
    input_wake_fd: i32,
    frame_wake: Option<desk_backend::FrameWake>,
) {
    let mut event_queue = event_queue;
    let qh = event_queue.handle();

    // A/B latency-experiment killswitches (e12): default behavior keeps the
    // e4/e5 wake-ups; setting either env var restores the pre-experiment
    // 8ms-poll behavior for side-by-side measurement. The input eventfd
    // still gets written peer-side (harmless), we just never poll it.
    let input_wake_fd = if std::env::var_os("WAYRDP_DISABLE_INPUT_WAKE").is_some() {
        -1
    } else {
        input_wake_fd
    };
    if let (Some(m), Some(seat)) = (&state.vptr_mgr, &state.seat) {
        state.vptr_v2 = m.version() >= 2;
        if !state.vptr_v2 {
            eprintln!(
                "[backend-wlroots] virtual-pointer v1; falling back to smooth axis for wheel (F8)"
            );
        }
        state.vptr = Some(m.create_virtual_pointer(Some(seat), &qh, ()));
    }
    if let (Some(m), Some(seat)) = (&state.vkbd_mgr, &state.seat) {
        state.vkbd = Some(m.create_virtual_keyboard(seat, &qh, ()));
    }
    ensure_keymap(&mut state);
    let _ = event_queue.roundtrip(&mut state);

    if state.frame_size.width == 0 {
        state.frame_size = initial;
    }
    eprintln!(
        "[backend-wlroots] shm-plugin pump live on output {} @ {}x{}",
        state.output_name, state.frame_size.width, state.frame_size.height
    );

    let mut shm: Option<ShmCapture> = None;
    // WAYRDP_DISABLE_FRAME_NOTIFY: e5 killswitch, see input wake above.
    let notify = if std::env::var_os("WAYRDP_DISABLE_FRAME_NOTIFY").is_some() {
        None
    } else {
        FrameNotify::bind(&state.output_name)
    };
    if notify.is_some() {
        eprintln!(
            "[backend-wlroots] frame notify socket live for {}",
            state.output_name
        );
    }
    let mut frames_sent: u64 = 0;
    let mut last_hist = std::time::Instant::now();
    let mut last_frame_at = std::time::Instant::now();
    // e12 latency stats: publish→pump-drain (queue wait between the plugin's
    // shm publish and our copy-out) and, per input event, peer-decode→pump-
    // drain (input queue wait). Sampled every frame/event, logged ~5s.
    let mut lat_pub = LatStats::default();
    let mut lat_in = LatStats::default();
    // Input → next published frame: the compositor's response (render +
    // plugin readback). Armed when an input batch is flushed, consumed by
    // the first frame the plugin stamped after it. Pointer motion that
    // repaints nothing (client-drawn cursor) never produces a frame, so an
    // armed stamp expires after RESPONSE_WINDOW_MS rather than pairing with
    // some unrelated later frame.
    const RESPONSE_WINDOW_MS: u32 = 250;
    let mut lat_resp = LatStats::default();
    let mut input_flushed_at: Option<u32> = None;
    let trace_on = std::env::var_os("WAYRDP_DEBUG_CAPTURE").is_some()
        || std::env::var_os("WAYRDP_TRACE").is_some();
    let marker = std::env::var("WAYRDP_TRACE_MARKER")
        .ok()
        .and_then(|v| parse_trace_marker(&v))
        .filter(|_| trace_on);
    // (target x, target y, injected at): the newest motion supersedes, so a
    // sample is "this position's marker became visible".
    let mut marker_armed: Option<(i32, i32, u32)> = None;
    let mut lat_marker = LatStats::default();
    const MARKER_WINDOW_MS: u32 = 1000;
    let mut win_frames: u64 = 0;
    // e2a stats: how much of the surface do compositor damage rects cover?
    // Feeds the e3 two-tier (planar/RFX) threshold decision.
    let mut win_dmg_pct_sum: f64 = 0.0;
    let mut win_dmg_granular: u64 = 0;
    // Client-drawn cursor: asked of the plugin through the frame shm, image
    // read from the plugin's cursor shm.
    let mut cursor_requested = false;
    let mut cursor_unsupported_logged = false;
    let mut shm_nowhere_logged = false;
    let mut plugin_cursor: Option<PluginCursor> = None;
    let mut cursor_open_at = std::time::Instant::now();

    macro_rules! inject_ev {
        ($ev:expr) => {{
            let mut _drop_last_pos: Option<(f64, f64)> = None;
            inject(&mut state, &($ev), &mut _drop_last_pos);
        }};
    }

    'main: loop {
        // Resize latest-wins (shm variant): recreate output, drop/recreate shm.
        let pending_resize = resize_slot.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(sz) = pending_resize {
            destroy_headless_output(&state.output_name);
            create_headless_output(sz, &state.output_name, state.output_scale);
            sync_layout(&mut state);
            state.frame_size = sz;
            settle_after_create(&state.output_name);
            // Post-settle mode re-pin + re-sync (see startup note): the pin
            // applied during create_headless_output races Hyprland's async
            // default-mode assignment and loses; without the repeat the
            // extent stays at the default mode (1080 vs the pinned 1200)
            // and skews every injected absolute motion.
            repin_output_mode(&state.output_name, sz, state.output_scale, output_hz());
            sync_layout(&mut state);
            // Force shm reopen: geometry may differ.
            shm = None;
        }

        // Drain the input queue, coalescing motion events. StampedInput
        // stamps measure peer-decode→pump-drain latency (0 = unstamped).
        let mut coalesced_motion: Option<(i32, i32)> = None;
        let mut got_input = false;
        while let Ok(sev) = input_rx.try_recv() {
            if sev.stamp_ms != 0 {
                lat_in.push(now_mono_ms_u32().wrapping_sub(sev.stamp_ms));
            }
            match sev.event {
                InputEvent::PointerMotion { x, y } => {
                    coalesced_motion = Some((x, y));
                    got_input = true;
                }
                other => {
                    if let Some((x, y)) = coalesced_motion.take() {
                        inject_ev!(InputEvent::PointerMotion { x, y });
                    }
                    inject_ev!(other);
                    got_input = true;
                }
            }
        }
        if let Some((x, y)) = coalesced_motion {
            inject_ev!(InputEvent::PointerMotion { x, y });
            if let Some(m) = marker {
                marker_armed = Some((x + m.dx, y + m.dy, now_mono_ms_u32()));
            }
        }
        // Input flush now — don't wait for the end-of-iteration `flush()`.
        // Every ms of delay between "mouse moved" and "compositor saw it"
        // is felt as lag.
        if got_input {
            let _ = event_queue.flush();
            if trace_on {
                input_flushed_at = Some(now_mono_ms_u32());
            }
        }

        // Client-drawn cursor: a changed image goes to the client right away
        // (the peer mainloop is woken; no frame needed). The plugin creates
        // its cursor shm when it sees the request, so keep trying to open it.
        if cursor_requested {
            if plugin_cursor.is_none() && cursor_open_at.elapsed().as_millis() >= 100 {
                cursor_open_at = std::time::Instant::now();
                if let Some(path) = PluginCursor::path_for(&state.output_name) {
                    plugin_cursor = PluginCursor::open(&path);
                }
            }
            if let Some(update) = plugin_cursor.as_mut().and_then(|c| c.poll()) {
                if state.cursor.publish(update) {
                    if let Some(wake) = &frame_wake {
                        wake();
                    }
                }
            }
        }

        // Disconnect/watchdog.
        match input_rx.try_recv() {
            Err(TryRecvError::Disconnected) => {
                // Teardown timeline marker: shm pump saw the peer drop the channel.
                eprintln!(
                    "[backend-wlroots] teardown {}: shm pump cmd channel hung up",
                    state.output_name
                );
                state.shutdown = true;
                break 'main;
            }
            Ok(sev) => inject_ev!(sev.event),
            Err(TryRecvError::Empty) => {}
        }

        // Open the shm if not yet open; the plugin may not have run yet.
        if shm.is_none() {
            let path = ShmCapture::path_for(&state.output_name);
            match path.map(|p| ShmCapture::open(&p)) {
                Some(Some(s)) => {
                    eprintln!(
                        "[backend-wlroots] shm opened for {}: {}x{}",
                        state.output_name, s.width, s.height
                    );
                    // Client-drawn cursor needs a plugin that leaves the
                    // cursor out and publishes its image. Asked on every
                    // open: a recreated frame file starts painted.
                    if state.cursor.slot.is_some() {
                        if s.supports_client_cursor() {
                            s.file.request_client_cursor(true);
                            if !cursor_requested {
                                eprintln!(
                                    "[backend-wlroots] client cursor on {}: plugin leaves it out of the frames",
                                    state.output_name
                                );
                            }
                            cursor_requested = true;
                        } else if !cursor_unsupported_logged {
                            eprintln!("[backend-wlroots] client cursor: capture plugin cannot hide the cursor (rebuild it); cursor stays painted");
                            cursor_unsupported_logged = true;
                        }
                    }
                    shm = Some(s);
                }
                Some(None) => {
                    // Plugin hasn't created the segment yet. Give it some
                    // time to appear (it only spawns when Hyprland renders
                    // a wayrdp monitor after plugin load).
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue 'main;
                }
                None => {
                    // No XDG_RUNTIME_DIR: there is no safe segment location
                    // (a world-writable fallback would re-open the squat
                    // attack this layout exists to prevent). Retry-park.
                    if !shm_nowhere_logged {
                        eprintln!(
                            "[backend-wlroots] XDG_RUNTIME_DIR unset; nowhere to open the capture segment"
                        );
                        shm_nowhere_logged = true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                    continue 'main;
                }
            }
        }

        // Poll for a new frame from the plugin. NOTE: rdpgfx's mainloop
        // wakes at most every 16ms and drains the channel latest-wins.
        // Sending faster than ~60Hz only burns a 9MB copy per frame that
        // is immediately discarded. Rate-limit: at most one send per tick.
        let cap_start = std::time::Instant::now();
        let got_frame = shm.as_mut().and_then(|s| s.poll_frame());
        let cap_ms = cap_start.elapsed().as_millis() as u64;

        if let Some(frame) = got_frame {
            // e12: publish→drain queue wait from the plugin's capture stamp
            // (same CLOCK_MONOTONIC domain; 0 = old unstamped plugin).
            let stamp = frame.meta.capture_stamp_ms;
            if let (Some(m), Some((x, y, t))) = (marker, marker_armed) {
                let at = if stamp != 0 { stamp } else { now_mono_ms_u32() };
                if marker_at(
                    &frame.pixels[..],
                    frame.meta.stride,
                    frame.meta.size,
                    x,
                    y,
                    m.rgb,
                ) {
                    lat_marker.push(at.wrapping_sub(t));
                    marker_armed = None;
                    // Per-sample record, joined offline with the test
                    // client's own receive/commit stamps on (x, y).
                    eprintln!(
                        "[trace-marker] x={} y={} inject={t} visible={at}",
                        x - m.dx,
                        y - m.dy
                    );
                } else if now_mono_ms_u32().wrapping_sub(t) >= MARKER_WINDOW_MS {
                    marker_armed = None;
                }
            }
            if stamp != 0 {
                lat_pub.push(now_mono_ms_u32().wrapping_sub(stamp));
                if let Some(t) = input_flushed_at {
                    let d = stamp.wrapping_sub(t);
                    // d >= 2^31: published before the flush (pre-input frame).
                    if d < RESPONSE_WINDOW_MS {
                        lat_resp.push(d);
                        input_flushed_at = None;
                    } else if d < 1 << 31 {
                        input_flushed_at = None;
                    }
                }
            }
            // e2a: accumulate damage coverage for the stats window. A frame
            // counts as "granular" when it did NOT fall back to the single
            // full-surface rect (legacy plugin or empty/oversized damage).
            {
                let full = frame.meta.damage.len() == 1
                    && frame.meta.damage[0].width == frame.meta.size.width
                    && frame.meta.damage[0].height == frame.meta.size.height;
                let area: u64 = frame
                    .meta
                    .damage
                    .iter()
                    .map(|r| r.width as u64 * r.height as u64)
                    .sum();
                win_dmg_pct_sum += area as f64
                    / (frame.meta.size.width as f64 * frame.meta.size.height as f64)
                    * 100.0;
                if !full {
                    win_dmg_granular += 1;
                }
            }
            if tx.send(frame).is_err() {
                break 'main; // encoder hung up
            }
            if let Some(wake) = &frame_wake {
                wake();
            }
            frames_sent += 1;
            win_frames += 1;
            last_frame_at = std::time::Instant::now();

            // Every 5s (and only under WAYRDP_DEBUG_CAPTURE), dump the
            // latency window: pub = plugin-publish→pump-drain, in = peer
            // decode→pump-drain. Values are avg/p95/max ms over the window.
            if last_hist.elapsed().as_secs() >= 5 && trace_on {
                let lp = lat_pub.take_summary();
                let li = lat_in.take_summary();
                let lr = lat_resp.take_summary();
                let lm = lat_marker.take_summary();
                eprintln!(
                    "[pump-shm] frames={} cap={}ms fps_win={:.1} pub_ms(avg/p95/max)={}/{}/{} over {} in_ms(avg/p95/max)={}/{}/{} over {} in→pub(p50/p95/max)={} over {} in→marker(p50/p95/max)={} over {} dmg_pct={:.1} dmg_gran={}/{}",
                    frames_sent,
                    cap_ms,
                    win_frames as f64 / last_hist.elapsed().as_secs_f64(),
                    lp.avg,
                    lp.p95,
                    lp.max,
                    lp.n,
                    li.avg,
                    li.p95,
                    li.max,
                    li.n,
                    lr,
                    lr.n,
                    lm,
                    lm.n,
                    win_dmg_pct_sum / win_frames.max(1) as f64,
                    win_dmg_granular,
                    win_frames,
                );
                win_frames = 0;
                win_dmg_pct_sum = 0.0;
                win_dmg_granular = 0;
                last_hist = std::time::Instant::now();
            }
        } else {
            if let Some(t) = input_flushed_at {
                if now_mono_ms_u32().wrapping_sub(t) >= RESPONSE_WINDOW_MS {
                    input_flushed_at = None;
                }
            }
            // No new frame: verify the shm isn't stale. On a resize the
            // plugin unlinks the old file and creates a new one; if we
            // mapped it in between, seq never advances again and the session
            // freezes at the old geometry. Inode check catches that.
            let stall = last_frame_at.elapsed().as_millis() >= 1000;
            if stall {
                // Rate-limit the syscall work inside is_stale: maybe_stale is
                // one fstat on the mapped fd; the full path stat runs only
                // when the file's ctime ticked (i.e. at most once per second
                // while the plugin recreates it, never on a healthy idle
                // desktop).
                let path = match ShmCapture::path_for(&state.output_name) {
                    Some(p) => p,
                    None => {
                        // No runtime dir: there is no safe segment location.
                        if !shm_nowhere_logged {
                            eprintln!(
                                "[backend-wlroots] XDG_RUNTIME_DIR unset; nowhere to open the capture segment"
                            );
                            shm_nowhere_logged = true;
                        }
                        last_frame_at = std::time::Instant::now();
                        continue 'main;
                    }
                };
                if shm
                    .as_ref()
                    .is_none_or(|s| s.maybe_stale() && s.is_stale(&path))
                {
                    eprintln!("[backend-wlroots] shm seq stalled and file stale; reopening");
                    shm = None;
                    last_frame_at = std::time::Instant::now();
                    continue 'main;
                }
            }
            // Event-driven idle wait instead of the 500µs spin (~2000 Hz of
            // flush+dispatch on an idle desktop): poll the wayland fd, the
            // plugin's frame-notify socket, and the input-wake eventfd. Wake
            // sources are keyboard repeat / compositor events, a published
            // frame, or a just-queued input event; otherwise the 8ms timeout
            // caps pump-side frame latency at ~120Hz polling, ample for a
            // 60Hz encoder. After injecting input this pass, don't sleep: the
            // compositor's feedback (e.g. keymap acks) is imminent and we
            // must dispatch it before the next inject.
            let _ = event_queue.flush();
            let woke_for_work = if let Some(guard) = event_queue.prepare_read() {
                let fd = guard.connection_fd().as_raw_fd();
                let notify_fd = notify.as_ref().map_or(-1, |n| n.fd);
                let timeout_ms = if got_input { 0 } else { 8 };
                let (wl_ev, notify_ev, wake_ev) =
                    libc_poll_trio(fd, notify_fd, input_wake_fd, timeout_ms);
                if notify_ev & (POLLIN as i32) != 0 {
                    // Plugin published a frame; skip the 8ms ceiling and poll
                    // the shm immediately on the next pass. The wayland read
                    // is not skipped — an idle poll cycle means nothing arrived
                    // here for it anyway, and any keymap acks surface via the
                    // wl fd too (handled in the same pass below).
                    if let Some(n) = &notify {
                        n.drain();
                    }
                }
                if wl_ev > 0 && wl_ev & (POLLIN as i32) != 0 {
                    if let Err(e) = guard.read() {
                        eprintln!("[backend-wlroots] shm pump wayland read error: {e:?}");
                        break 'main;
                    }
                } else {
                    drop(guard);
                }
                if wake_ev & (POLLIN as i32) != 0 {
                    // Input landed while we were idle: drain the eventfd so
                    // it stops being readable, and loop back to the channel
                    // drain + inject at the top of the pass.
                    eventfd_drain(input_wake_fd);
                }
                // A plugin notify or input wake with no wayland read pending
                // means "work now": loop straight back to poll_frame / the
                // input drain instead of waiting for the next poll cycle.
                (notify_ev | wake_ev) & (POLLIN as i32) != 0
            } else {
                false
            };
            if let Err(e) = event_queue.dispatch_pending(&mut state) {
                eprintln!("[backend-wlroots] shm pump wayland dispatch error: {e}");
                break 'main;
            }
            if woke_for_work {
                continue 'main;
            }
        }

        // Always service Wayland so input injects can flush.
        let _ = event_queue.flush();
        if let Err(e) = event_queue.dispatch_pending(&mut state) {
            eprintln!("[backend-wlroots] wayland dispatch error: {e}");
            break 'main;
        }
    }

    // Teardown: drop virtual input + headless output.
    if let Some(p) = state.vptr.take() {
        p.destroy();
    }
    drop(state.vkbd.take());
    if cursor_requested {
        if let Some(s) = &shm {
            s.file.request_client_cursor(false);
        }
    }
    let _ = event_queue.flush();
    live_output_remove(&state.output_name);
    destroy_headless_output(&state.output_name);
    eprintln!(
        "[backend-wlroots] teardown {}: shm output remove returned",
        state.output_name
    );
    eprintln!(
        "[backend-wlroots] shm pump exit; frames_sent={}",
        frames_sent
    );
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    #[test]
    fn shm_names_live_under_the_runtime_dir_and_refuse_without_one() {
        // Segments moved out of /dev/shm (mode 1777, listable: any local
        // user could read a name suffix and pre-create the next one) into
        // the 0700 user-owned runtime dir, where cross-user squatting is
        // impossible. The plugin derives the identical path. The param-
        // taking helper avoids mutating the process env (parallel tests).
        let rt = std::ffi::OsStr::new("/run/user/1000");
        assert_eq!(
            super::segment_path_in(rt, "capture", "wayrdp-7"),
            Some(format!("{}/wayrdp-capture-wayrdp-7", rt.to_str().unwrap()).into())
        );
        assert_eq!(
            super::segment_path_in(rt, "cursor", "wayrdp-7"),
            Some(format!("{}/wayrdp-cursor-wayrdp-7", rt.to_str().unwrap()).into())
        );
        assert_eq!(
            super::segment_path_in(std::ffi::OsStr::new(""), "capture", "wayrdp-7"),
            None
        );
    }

    #[test]
    fn live_output_registry_tracks_ownership() {
        // The sweep's liveness source: add before create, remove at
        // teardown, no duplicate entries, other names untouched.
        super::live_output_add("wayrdp-1");
        super::live_output_add("wayrdp-1"); // idempotent
        super::live_output_add("wayrdp-2");
        {
            let live = super::LIVE_OUTPUTS.lock().unwrap();
            assert!(live.iter().any(|n| n == "wayrdp-1"));
            assert!(live.iter().any(|n| n == "wayrdp-2"));
            assert_eq!(live.iter().filter(|n| **n == "wayrdp-1").count(), 1);
        }
        super::live_output_remove("wayrdp-1");
        {
            let live = super::LIVE_OUTPUTS.lock().unwrap();
            assert!(!live.iter().any(|n| n == "wayrdp-1"));
            assert!(live.iter().any(|n| n == "wayrdp-2"));
        }
        super::live_output_remove("wayrdp-2");
        assert!(super::LIVE_OUTPUTS.lock().unwrap().is_empty());
    }

    #[test]
    fn trace_marker_parses_offset_and_colour() {
        assert_eq!(
            super::parse_trace_marker("60,0,ff00ff"),
            Some(super::TraceMarker {
                dx: 60,
                dy: 0,
                rgb: [0xff, 0, 0xff]
            })
        );
        assert_eq!(
            super::parse_trace_marker(" -4 , 7 , 00FF10 ").map(|m| (m.dx, m.dy, m.rgb)),
            Some((-4, 7, [0, 0xff, 0x10]))
        );
        for bad in [
            "",
            "60,0",
            "60,0,ff00f",
            "60,0,ff00ff,1",
            "x,0,ff00ff",
            "60,0,gg00ff",
        ] {
            assert_eq!(super::parse_trace_marker(bad), None, "{bad}");
        }
    }

    #[test]
    fn marker_matches_bgra_with_tolerance_and_bounds() {
        let size = desk_backend::Size {
            width: 2,
            height: 2,
        };
        // stride 12: 2 px + 4 bytes row padding
        let mut px = vec![0u8; 24];
        // (1,1) = magenta, slightly off, in BGRA
        px[12 + 4..12 + 8].copy_from_slice(&[250, 5, 251, 255]);
        let magenta = [0xff, 0x00, 0xff];
        assert!(super::marker_at(&px, 12, size, 1, 1, magenta));
        assert!(!super::marker_at(&px, 12, size, 0, 1, magenta));
        assert!(!super::marker_at(&px, 12, size, 2, 1, magenta));
        assert!(!super::marker_at(&px, 12, size, -1, 0, magenta));
        assert!(!super::marker_at(&px[..14], 12, size, 1, 1, magenta));
        assert!(!super::marker_at(&px, 12, size, 1, 1, [0xff, 0x00, 0xe0]));
    }

    use super::lcid_to_xkb;

    fn v2_header(caps: u32) -> Vec<u8> {
        let mut h = vec![0u8; super::ShmCapture::HDR_SIZE_V2];
        h[super::CAPS_OFFSET..super::CAPS_OFFSET + 4].copy_from_slice(&caps.to_ne_bytes());
        h
    }

    #[test]
    fn client_cursor_needs_the_plugin_capability() {
        let v2 = super::ShmCapture::HDR_SIZE_V2;
        assert!(super::plugin_supports_client_cursor(
            v2,
            &v2_header(super::CAP_CLIENT_CURSOR)
        ));
        // A plugin built before the handshake leaves the caps word zero.
        assert!(!super::plugin_supports_client_cursor(v2, &v2_header(0)));
        assert!(!super::plugin_supports_client_cursor(v2, &v2_header(2)));
    }

    #[test]
    fn client_cursor_never_read_from_pre_v2_headers() {
        // v1/legacy files have pixels at 160/24: byte 184 is pixel data.
        let px = v2_header(u32::MAX);
        assert!(!super::plugin_supports_client_cursor(
            super::ShmCapture::HDR_SIZE_V1,
            &px
        ));
        assert!(!super::plugin_supports_client_cursor(24, &px));
        assert!(!super::plugin_supports_client_cursor(
            super::ShmCapture::HDR_SIZE_V2,
            &px[..186]
        ));
    }

    #[test]
    fn cursor_handshake_words_match_the_plugin_layout() {
        // plugins/hyprland-capture/main.cpp HdrTail: releasedSeq ends at
        // 184, caps @184, cursorReq @188, "UCSR" request magic.
        assert_eq!(super::REL0_OFFSET + 16, super::CAPS_OFFSET);
        assert_eq!(super::CAPS_OFFSET + 4, super::CURSOR_REQ_OFFSET);
        assert_eq!(super::CURSOR_REQ_CLIENT, 0x5253_4355);
    }

    /// A plugin cursor shm snapshot (CursorHeader + rows).
    fn cursor_shm(w: u32, h: u32, hot: (i32, i32), visible: u32, px: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        let mut put = |o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_ne_bytes());
        put(0, super::PluginCursor::MAGIC);
        put(4, 1);
        put(16, w);
        put(20, h);
        put(24, hot.0 as u32);
        put(28, hot.1 as u32);
        put(32, visible);
        b[8..16].copy_from_slice(&2u64.to_ne_bytes());
        b.extend_from_slice(px);
        b
    }

    #[test]
    fn plugin_cursor_decodes_shape_and_hotspot() {
        // 2x1: opaque blue, half-transparent premultiplied red.
        let px = [255, 0, 0, 255, 0, 0, 64, 128];
        let Some(desk_backend::CursorUpdate::Shape(img)) =
            super::parse_plugin_cursor(&cursor_shm(2, 1, (1, 0), 1, &px))
        else {
            panic!("expected a shape");
        };
        assert_eq!((img.width, img.height, img.hot_x, img.hot_y), (2, 1, 1, 0));
        assert_eq!(&img.bgra[..4], &[255, 0, 0, 255]);
        assert_eq!(img.bgra[7], 128);
        assert_eq!(img.bgra[6], 128); // un-premultiplied: 64 * 255 / 128
    }

    #[test]
    fn plugin_cursor_hidden_and_unusable_snapshots() {
        assert_eq!(
            super::parse_plugin_cursor(&cursor_shm(0, 0, (0, 0), 0, &[])),
            Some(desk_backend::CursorUpdate::Hidden)
        );
        // Not written yet (zero magic), truncated pixels, oversized.
        assert_eq!(super::parse_plugin_cursor(&[0u8; 64]), None);
        assert_eq!(
            super::parse_plugin_cursor(&cursor_shm(4, 4, (0, 0), 1, &[0; 8])),
            None
        );
        assert_eq!(
            super::parse_plugin_cursor(&cursor_shm(512, 1, (0, 0), 1, &[0; 2048])),
            None
        );
    }

    #[test]
    fn lcid_maps_common_layouts() {
        assert_eq!(lcid_to_xkb(0x0409), ("us", ""));
        assert_eq!(lcid_to_xkb(0x20409), ("us", "intl"));
        assert_eq!(lcid_to_xkb(0x0413), ("nl", ""));
        assert_eq!(lcid_to_xkb(0x0813), ("be", ""));
        assert_eq!(lcid_to_xkb(0x0407), ("de", ""));
        assert_eq!(lcid_to_xkb(0x040C), ("fr", ""));
        assert_eq!(lcid_to_xkb(0x0809), ("gb", ""));
        assert_eq!(lcid_to_xkb(0x0416), ("br", ""));
    }

    #[test]
    fn lcid_language_instance_falls_back_to_language() {
        // Some clients set the HIWORD to a layout-instance id (e.g.
        // 0xF0xx0413). The language-only retry must still land on Dutch.
        assert_eq!(lcid_to_xkb(0xF0DE0413 & 0xF0FF_FFFF | 0x000F_0413).0, "nl");
        assert_eq!(lcid_to_xkb(0x0001_0413), ("nl", ""));
    }

    #[test]
    fn lcid_unknown_defaults_to_us() {
        assert_eq!(lcid_to_xkb(0xFFFF), ("us", ""));
        // 0 = client didn't advertise; caller guards this, but be safe.
        assert_eq!(lcid_to_xkb(0), ("us", ""));
    }

    fn leak_rect_payload(count: u32, rects: &[(u32, u32, u32, u32)]) -> &'static [u8] {
        let mut v = Vec::with_capacity(4 + rects.len() * 16);
        v.extend_from_slice(&count.to_ne_bytes());
        for r in rects {
            for f in [r.0, r.1, r.2, r.3] {
                v.extend_from_slice(&f.to_ne_bytes());
            }
        }
        Box::leak(v.into_boxed_slice())
    }

    #[test]
    fn damage_rects_parse_valid_set() {
        // 160-byte v1 payload region (count + 8 slots), as the reader sees it.
        let buf = leak_rect_payload(2, &[(0, 0, 100, 50), (1920 - 64, 1200 - 32, 64, 32)]);
        let rs = super::parse_damage_rects(buf, 1920, 1200).expect("valid set");
        assert_eq!(rs.len(), 2);
        assert_eq!(
            (rs[0].x, rs[0].y, rs[0].width, rs[0].height),
            (0, 0, 100, 50)
        );
        assert_eq!(
            (rs[1].x, rs[1].y, rs[1].width, rs[1].height),
            (1856, 1168, 64, 32)
        );
    }

    #[test]
    fn damage_rects_count_zero_means_full_surface() {
        let buf = leak_rect_payload(0, &[]);
        assert!(super::parse_damage_rects(buf, 1920, 1200).is_none());
    }

    #[test]
    fn damage_rects_reject_out_of_bounds() {
        // One bad rect discards the whole set — partial damage is worse
        // than full-surface (a missed rect shows as a stale screen patch).
        let buf = leak_rect_payload(2, &[(0, 0, 100, 50), (1900, 0, 100, 50)]);
        assert!(super::parse_damage_rects(buf, 1920, 1200).is_none());
    }

    #[test]
    fn damage_rects_reject_wrapping_extent() {
        // x + w overflowing u32 must not slip past the bounds check.
        let buf = leak_rect_payload(1, &[(u32::MAX - 3, 0, 100, 50)]);
        assert!(super::parse_damage_rects(buf, 1920, 1200).is_none());
    }

    #[test]
    fn damage_rects_reject_impossible_count() {
        let buf = leak_rect_payload(9, &[]);
        assert!(super::parse_damage_rects(buf, 1920, 1200).is_none());
    }

    #[test]
    fn damage_rects_reject_zero_area_rect() {
        let buf = leak_rect_payload(1, &[(10, 10, 0, 50)]);
        assert!(super::parse_damage_rects(buf, 1920, 1200).is_none());
    }

    #[test]
    fn cursor_image_unpremultiplies_argb() {
        use wayland_client::protocol::wl_shm::Format;
        // B,G,R,A premultiplied: opaque, half-alpha, transparent, and an
        // over-range channel (invalid premultiplication) that must clamp.
        let px = [10, 20, 30, 255, 50, 25, 0, 128, 9, 9, 9, 0, 200, 0, 0, 100];
        let size = desk_backend::Size {
            width: 2,
            height: 2,
        };
        let img = super::cursor_image_from_shm(&px, size, Format::Argb8888, (1, 5));
        assert_eq!(
            img.bgra,
            vec![10, 20, 30, 255, 100, 50, 0, 128, 0, 0, 0, 0, 255, 0, 0, 100]
        );
        assert_eq!(
            (img.hot_x, img.hot_y),
            (1, 1),
            "hotspot clamps into the image"
        );
    }

    #[test]
    fn cursor_image_from_xrgb_is_opaque() {
        use wayland_client::protocol::wl_shm::Format;
        let px = [1, 2, 3, 0];
        let size = desk_backend::Size {
            width: 1,
            height: 1,
        };
        let img = super::cursor_image_from_shm(&px, size, Format::Xrgb8888, (-3, 0));
        assert_eq!(img.bgra, vec![1, 2, 3, 255]);
        assert_eq!((img.hot_x, img.hot_y), (0, 0));
    }
}
