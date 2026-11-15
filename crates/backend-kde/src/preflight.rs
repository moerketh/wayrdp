//! Startup checks for the KDE backend, run once before the listener opens.
//!
//! Permanent failures (Plasma too old, tools missing) are configuration
//! errors the server turns into exit status 42 so systemd stops retrying;
//! transient ones (KWin not on the bus yet at login) exit normally so the
//! unit's restart loop gets another go.

use std::process::Command;
use std::time::Duration;

/// Oldest KWin the backend is validated against (Parrot's Plasma 6.3.6).
/// Older KWin lacks `ext_data_control` and in-place mode changes, and its
/// zkde screencast behaves differently enough to be untested.
pub const MIN_KWIN: (u32, u32) = (6, 3);

#[derive(Debug)]
pub enum Failure {
    /// Will not fix itself on retry.
    Permanent(String),
    /// Probably the session is still starting.
    Transient(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Permanent(s) | Failure::Transient(s) => f.write_str(s),
        }
    }
}

/// Parse `kwin_wayland --version` output ("kwin_wayland 6.3.6", "kwin 6.7.4").
pub fn parse_kwin_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = word
        .split(|c: char| !c.is_ascii_digit())
        .map(|p| p.parse::<u32>().ok());
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

pub fn version_supported(v: (u32, u32, u32)) -> bool {
    (v.0, v.1) >= MIN_KWIN
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let p = dir.join(tool);
            std::fs::metadata(&p).is_ok_and(|m| m.is_file())
        })
    })
}

fn kwin_version() -> Option<(u32, u32, u32)> {
    let out = Command::new("kwin_wayland")
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    parse_kwin_version(&text).or_else(|| parse_kwin_version(&err))
}

fn kwin_on_bus() -> Result<bool, String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| format!("session bus: {e}"))?;
    let dbus = zbus::blocking::fdo::DBusProxy::new(&conn).map_err(|e| e.to_string())?;
    let name = zbus::names::BusName::try_from("org.kde.KWin").map_err(|e| e.to_string())?;
    dbus.name_has_owner(name).map_err(|e| e.to_string())
}

/// Check what the backend needs before any client connects.
pub fn preflight() -> Result<(), Failure> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(Failure::Transient(
            "WAYLAND_DISPLAY is not set (start wayrdp inside the Plasma Wayland session)".into(),
        ));
    }
    // Tools hyperv-rdp-extras shells out to (Debian packages in brackets).
    for (tool, package) in [
        ("kscreen-doctor", "kscreen"),
        ("gdbus", "libglib2.0-bin"),
        ("pkill", "procps"),
    ] {
        if !on_path(tool) {
            return Err(Failure::Permanent(format!(
                "{tool} not found: install {package} (the KDE backend needs it)"
            )));
        }
    }
    match kwin_version() {
        Some(v) if !version_supported(v) => {
            return Err(Failure::Permanent(format!(
                "KWin {}.{}.{} is too old: the KDE backend needs Plasma {}.{} or newer",
                v.0, v.1, v.2, MIN_KWIN.0, MIN_KWIN.1
            )));
        }
        Some(v) => eprintln!("[backend-kde] preflight: KWin {}.{}.{}", v.0, v.1, v.2),
        None => eprintln!("[backend-kde] preflight: KWin version unknown; continuing"),
    }
    // KWin claims its bus name a moment after WAYLAND_DISPLAY appears.
    let mut last = String::new();
    for _ in 0..10 {
        match kwin_on_bus() {
            Ok(true) => return Ok(()),
            Ok(false) => last = "org.kde.KWin is not on the session bus".into(),
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(Failure::Transient(format!(
        "{last} (is this a KWin session?)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kwin_version_strings() {
        assert_eq!(parse_kwin_version("kwin_wayland 6.3.6\n"), Some((6, 3, 6)));
        assert_eq!(parse_kwin_version("kwin 6.7.4"), Some((6, 7, 4)));
        assert_eq!(
            parse_kwin_version("kwin_wayland 6.4.90-1"),
            Some((6, 4, 90))
        );
        assert_eq!(parse_kwin_version("kwin 6"), Some((6, 0, 0)));
        assert_eq!(parse_kwin_version("no version here"), None);
    }

    #[test]
    fn minimum_is_plasma_6_3() {
        assert!(version_supported((6, 3, 0)));
        assert!(version_supported((6, 7, 4)));
        assert!(version_supported((7, 0, 0)));
        assert!(!version_supported((6, 2, 5)));
        assert!(!version_supported((5, 27, 11)));
    }
}
