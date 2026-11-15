//! KDE Plasma backend for wayrdp.
//!
//! A thin adapter that drives `hyperv-rdp-extras`'s `kwin-virtual` session
//! machinery behind the `desk-backend` traits. All Plasma-specific behavior
//! (virtual outputs, in-place mode change, layout guard, containment heal)
//! stays inside extras — this crate must never fork that logic. Capture
//! (PipeWire, SHM only) and libei input injection are authored here.
//!
//! Threads per session:
//! - `kde-session`: current-thread tokio runtime for the extras calls,
//!   health and resize coalescing (`session.rs`);
//! - `kde-pipewire`: PipeWire main loop, copies frames into `FrameTx`
//!   (`capture.rs`);
//! - `kde-input`: EIS poll loop (`input.rs`).

pub mod capture;
pub mod health;
pub mod input;
pub mod input_map;
pub mod recovery;
pub mod resize;
pub mod session;

use std::sync::Arc;
use std::time::{Duration, Instant};

use desk_backend::{
    BackendError, CaptureSession, CaptureTarget, DesktopBackend, FrameTx, FrameWake,
    SessionRequest, Size, StampedInput,
};

use crate::input::Input;
use crate::session::Control;

/// How long a dropped session waits for the ordered teardown (guard
/// restore + close) before detaching it. The guard's own Drop still
/// restores physical outputs if the thread outlives us.
const TEARDOWN_WAIT: Duration = Duration::from_secs(8);

pub struct KdeBackend;

impl DesktopBackend for KdeBackend {
    fn name(&self) -> &'static str {
        "kde"
    }

    fn supports_dynamic_resize(&self) -> bool {
        true // KWin virtual outputs — the whole point of the extras library
    }

    /// No I/O: runs on the peer thread. Everything is set up in `start`.
    fn open(&self, req: SessionRequest) -> desk_backend::Result<Box<dyn CaptureSession>> {
        let size = match req.target {
            CaptureTarget::NewVirtualOutput(s) => s,
            CaptureTarget::ExistingOutput { .. } => {
                return Err(BackendError::Unsupported(
                    "kde backend only streams its own virtual output",
                ))
            }
        };
        let size = if size.width == 0 || size.height == 0 {
            req.initial_size
        } else {
            size
        };
        if req.keyboard_layout != 0 {
            // The guest's own layout applies (EIS sends evdev codes through
            // KWin's keymap); VMCreate provisions it to match the host.
            eprintln!(
                "[backend-kde] client keyboard LCID 0x{:04x}; keys follow the guest's KWin layout",
                req.keyboard_layout
            );
        }
        Ok(Box::new(KdeCaptureSession {
            size: resize::sanitize(size).0,
            wake: None,
            control: Arc::new(Control::default()),
            input: None,
            session: None,
            capture: None,
        }))
    }

    fn open_clipboard(&self) -> Option<Box<dyn desk_backend::Clipboard>> {
        Some(Box::new(clipboard_wayland::WlClipboard::new()))
    }
}

pub struct KdeCaptureSession {
    size: Size,
    wake: Option<FrameWake>,
    control: Arc<Control>,
    input: Option<Arc<Input>>,
    session: Option<std::thread::JoinHandle<()>>,
    capture: Option<std::thread::JoinHandle<()>>,
}

impl CaptureSession for KdeCaptureSession {
    fn start(&mut self, tx: FrameTx) -> desk_backend::Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let io = |e: std::io::Error| BackendError::Other(e.into());
        let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cmd, capture) = capture::spawn(tx, self.wake.clone(), ev_tx).map_err(io)?;
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
            control: self.control.clone(),
            capture: cmd.clone(),
            capture_events: ev_rx,
            input: input.clone(),
        };
        let session = match std::thread::Builder::new()
            .name("kde-session".into())
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
            eprintln!("[backend-kde] teardown: {what} still running; detaching");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = handle.join();
}

impl Drop for KdeCaptureSession {
    fn drop(&mut self) {
        self.control.request_shutdown();
        let deadline = Instant::now() + TEARDOWN_WAIT;
        if let Some(h) = self.session.take() {
            join_by(h, deadline, "session thread");
        }
        // The session thread already shut input down; this is a no-op
        // unless it was detached.
        if let Some(i) = self.input.take() {
            i.shutdown();
        }
        if let Some(h) = self.capture.take() {
            join_by(h, deadline + Duration::from_secs(1), "capture thread");
        }
    }
}
