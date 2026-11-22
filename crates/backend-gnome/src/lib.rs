//! GNOME (Mutter) backend for wayrdp.
//!
//! Mutter creates the virtual monitor itself: a RemoteDesktop session, a
//! linked ScreenCast session and `RecordVirtual` give a PipeWire node on the
//! shared daemon, and the PipeWire offer's size becomes the monitor's size.
//! Input goes through the same session's `ConnectToEIS`. No portal, no
//! consent dialog (spike S0, Mutter 50.5). Capture and input are the shared
//! `capture-pipewire` / `input-eis` code the KDE backend uses.
//!
//! Threads per session:
//! - `gnome-session`: current-thread tokio runtime owning the Mutter session,
//!   layout guard, health and resize (`session.rs`);
//! - `gnome-pipewire`: PipeWire main loop (capture-pipewire);
//! - `gnome-input`: EIS poll loop (input-eis).
//!
//! Unlike KDE there is no shell to restart: gnome-shell is the compositor,
//! so a heal re-applies the layout guard and the strongest repair is a new
//! Mutter session.

pub mod input;
pub mod layout;
pub mod mutter;
pub mod preflight;
pub mod session;

use std::sync::Arc;
use std::time::{Duration, Instant};

use capture_pipewire::capture;
use desk_backend::resize;
use desk_backend::{
    BackendError, CaptureSession, CaptureTarget, DesktopBackend, FrameTx, FrameWake,
    SessionRequest, Size, StampedInput,
};

use crate::input::Input;
use crate::session::Control;

const CAPTURE: capture::Config = capture::Config {
    thread: "gnome-pipewire",
    tag: "[backend-gnome]",
    stream_name: "wayrdp-gnome",
};

/// How long a dropped session waits for the ordered teardown before
/// detaching it. Mutter restores the console itself if we are gone.
const TEARDOWN_WAIT: Duration = Duration::from_secs(8);

pub struct GnomeBackend;

impl DesktopBackend for GnomeBackend {
    fn name(&self) -> &'static str {
        "gnome"
    }

    fn supports_dynamic_resize(&self) -> bool {
        true
    }

    /// No I/O: runs on the peer thread. Everything is set up in `start`.
    fn open(&self, req: SessionRequest) -> desk_backend::Result<Box<dyn CaptureSession>> {
        let size = match req.target {
            CaptureTarget::NewVirtualOutput(s) => s,
            CaptureTarget::ExistingOutput { .. } => {
                return Err(BackendError::Unsupported(
                    "gnome backend only streams its own virtual monitor",
                ))
            }
        };
        let size = if size.width == 0 || size.height == 0 {
            req.initial_size
        } else {
            size
        };
        if req.keyboard_layout != 0 {
            eprintln!(
                "[backend-gnome] client keyboard LCID 0x{:04x}; keys follow the guest's layout",
                req.keyboard_layout
            );
        }
        let scale = (req.desktop_scale_pct.clamp(100, 300) as f64) / 100.0;
        Ok(Box::new(GnomeCaptureSession {
            size: resize::sanitize(size).0,
            scale,
            wake: None,
            control: Arc::new(Control::default()),
            input: None,
            session: None,
            capture: None,
        }))
    }

    /// Clipboard comes later (WP9).
    fn open_clipboard(&self) -> Option<Box<dyn desk_backend::Clipboard>> {
        None
    }
}

pub struct GnomeCaptureSession {
    size: Size,
    scale: f64,
    wake: Option<FrameWake>,
    control: Arc<Control>,
    input: Option<Arc<Input>>,
    session: Option<std::thread::JoinHandle<()>>,
    capture: Option<std::thread::JoinHandle<()>>,
}

impl CaptureSession for GnomeCaptureSession {
    fn start(&mut self, tx: FrameTx) -> desk_backend::Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let io = |e: std::io::Error| BackendError::Other(e.into());
        let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cmd, capture) = capture::spawn(CAPTURE, tx, self.wake.clone(), ev_tx).map_err(io)?;
        let input = match Input::spawn() {
            Ok(i) => Arc::new(i),
            Err(e) => {
                let _ = cmd.send(capture::Command::Shutdown);
                let _ = capture.join();
                return Err(io(e));
            }
        };
        let s = session::Session {
            initial: self.size,
            scale: self.scale,
            control: self.control.clone(),
            capture: cmd.clone(),
            capture_events: ev_rx,
            input: input.clone(),
        };
        let session = match std::thread::Builder::new()
            .name("gnome-session".into())
            .spawn(move || session::run(s))
        {
            Ok(h) => h,
            Err(e) => {
                input.shutdown();
                let _ = cmd.send(capture::Command::Shutdown);
                let _ = capture.join();
                return Err(io(e));
            }
        };
        self.input = Some(input);
        self.session = Some(session);
        self.capture = Some(capture);
        Ok(())
    }

    /// Latest-wins slot; the session thread debounces and applies it.
    fn resize(&mut self, size: Size) -> desk_backend::Result<()> {
        self.control.request_resize(size);
        Ok(())
    }

    fn inject(&mut self, input: StampedInput) -> desk_backend::Result<()> {
        if let Some(i) = &self.input {
            i.inject(input.event);
        }
        Ok(())
    }

    fn set_frame_wake(&mut self, wake: FrameWake) {
        self.wake = Some(wake);
    }
}

fn join_by(handle: std::thread::JoinHandle<()>, deadline: Instant, what: &str) {
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            eprintln!("[backend-gnome] teardown: {what} still running; detaching");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = handle.join();
}

impl Drop for GnomeCaptureSession {
    fn drop(&mut self) {
        self.control.request_shutdown();
        let deadline = Instant::now() + TEARDOWN_WAIT;
        if let Some(h) = self.session.take() {
            join_by(h, deadline, "session thread");
        }
        if let Some(i) = self.input.take() {
            i.shutdown();
        }
        if let Some(h) = self.capture.take() {
            join_by(h, deadline + Duration::from_secs(1), "capture thread");
        }
    }
}

/// At startup: if a previous run left the console without a physical
/// monitor and no virtual monitor is around (Mutter normally restores it
/// itself), bring the physical monitors back.
pub fn recover_at_startup() {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    rt.block_on(async {
        let Ok(conn) = zbus::Connection::session().await else {
            return;
        };
        let Ok(st) = mutter::state(&conn).await else {
            return;
        };
        let virtual_present = st.monitors.iter().any(|m| m.connector.starts_with("Meta-"));
        if !layout::physical_enabled(&st) && !virtual_present {
            eprintln!("[backend-gnome] recovery: no physical monitor enabled; restoring");
            session::restore(&conn).await;
        }
    });
}
