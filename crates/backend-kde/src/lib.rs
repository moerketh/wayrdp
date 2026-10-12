//! KDE Plasma backend for wayrdp.
//!
//! Phase 1 work item: thin adapter that drives `hyperv-rdp-extras`'s
//! `kwin-virtual` session machinery behind the `desk-backend` traits.
//! All Plasma-specific behavior (virtual outputs, in-place mode change,
//! layout guard, containment heal) stays inside extras — this crate must
//! never fork that logic. libei input injection is authored fresh here.

use desk_backend::{BackendError, DesktopBackend, SessionRequest};

/// Placeholder so dependency wiring is exercised from day one. The real
/// `DesktopBackend` impl lands with Phase 1.
pub struct KdeBackend;

impl DesktopBackend for KdeBackend {
    fn name(&self) -> &'static str {
        "kde"
    }

    fn supports_dynamic_resize(&self) -> bool {
        true // KWin virtual outputs — the whole point of the extras library
    }

    fn open(
        &self,
        _req: SessionRequest,
    ) -> desk_backend::Result<Box<dyn desk_backend::CaptureSession>> {
        // Not a todo!(): a panic here unwinds into peer_accepted's FFI
        // context; the caller degrades to "session unavailable" on Err.
        Err(BackendError::Unsupported(
            "kde backend ships in Phase 1 (hyperv-rdp-extras not wired yet); \
             use WAYRDP_BACKEND=synthetic or wlroots",
        ))
    }
}
