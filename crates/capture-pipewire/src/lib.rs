//! SHM-only PipeWire capture plus the pure blank-desktop health monitor.
//!
//! Shared by the KDE (KWin zkde-screencast) and GNOME (Mutter ScreenCast)
//! backends: both publish a node on the user's shared PipeWire daemon, and
//! both must stay off DmaBuf on Hyper-V (see [`capture`]).

pub mod capture;
pub mod health;
