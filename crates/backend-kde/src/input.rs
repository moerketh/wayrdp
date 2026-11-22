//! KDE input: KWin's EIS endpoint, fed to the shared `input-eis` thread.
//!
//! `org.kde.KWin.EIS.RemoteDesktop.connectToEIS` hands a non-portal client a
//! libei socket with no consent dialog (KWin ≥ 6.1). KWin ties that EIS
//! context to the caller's D-Bus name, so the connection that asked for it
//! lives in the link's close hook until the link is closed.

use std::os::fd::OwnedFd;

pub use input_eis::Input;
use input_eis::{Config, EisLink};

use crate::recovery::KSCREEN_NAME;

const EIS_PATH: &str = "/org/kde/KWin/EIS/RemoteDesktop";
const EIS_IFACE: &str = "org.kde.KWin.EIS.RemoteDesktop";
/// connectToEIS capability bits (KWin's portal values): keyboard | pointer.
const EIS_CAPS: i32 = 1 | 2;

const CONFIG: Config = Config {
    thread: "kde-input",
    tag: "[backend-kde]",
};

fn connect() -> anyhow::Result<EisLink> {
    let dbus = zbus::blocking::Connection::session()?;
    let reply = dbus.call_method(
        Some("org.kde.KWin"),
        EIS_PATH,
        Some(EIS_IFACE),
        "connectToEIS",
        &(EIS_CAPS,),
    )?;
    let (fd, cookie): (zbus::zvariant::OwnedFd, i32) = reply.body().deserialize()?;
    let fd: OwnedFd = fd.into();
    Ok(EisLink {
        fd,
        label: format!("cookie {cookie}"),
        on_close: Some(Box::new(move || {
            let _ = dbus.call_method(
                Some("org.kde.KWin"),
                EIS_PATH,
                Some(EIS_IFACE),
                "disconnect",
                &(cookie,),
            );
        })),
    })
}

/// Spawn the input thread against KWin's EIS endpoint.
pub fn spawn() -> std::io::Result<Input> {
    Input::spawn(CONFIG, KSCREEN_NAME.to_string(), Box::new(connect))
}
