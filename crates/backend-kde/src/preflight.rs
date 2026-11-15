//! Startup checks for the KDE backend, run once before the listener opens.
//!
//! Permanent failures (Plasma too old, tools missing) are configuration
//! errors the server turns into exit status 42 so systemd stops retrying;
//! transient ones (KWin not on the bus yet at login) exit normally so the
//! unit's restart loop gets another go.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
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

/// The Wayland global the kde backend needs a desktop-file grant for.
pub const SCREENCAST_GLOBAL: &str = "zkde_screencast_unstable_v1";

/// Whether a desktop file grants `exe` the screencast global, the way
/// KWin's old check reads it: the first `Exec=` token, canonicalized, must
/// equal the client's executable, and `X-KDE-Wayland-Interfaces` must list
/// the global. `canon` resolves the Exec token (injected for tests).
pub fn desktop_grants(text: &str, exe: &Path, canon: impl Fn(&Path) -> Option<PathBuf>) -> bool {
    let mut exec = None;
    let mut grants = false;
    let mut in_entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(v) = line.strip_prefix("Exec=") {
            exec = v
                .split_whitespace()
                .next()
                .map(|t| t.trim_matches(|c| c == '"' || c == '\'').to_owned());
        } else if let Some(v) = line.strip_prefix("X-KDE-Wayland-Interfaces=") {
            grants = v.split([',', ';']).any(|i| i.trim() == SCREENCAST_GLOBAL);
        }
    }
    grants
        && exec
            .and_then(|e| canon(Path::new(&e)))
            .is_some_and(|p| p == exe)
}

/// Search `$XDG_DATA_HOME` and `$XDG_DATA_DIRS` `applications/` for a grant.
fn screencast_granted() -> bool {
    let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|e| std::fs::canonicalize(e).ok())
    else {
        return true; // can't tell; don't warn
    };
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let roots = home.into_iter().chain(dirs.split(':').map(PathBuf::from));
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root.join("applications")) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "desktop") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if desktop_grants(&text, &exe, |p| std::fs::canonicalize(p).ok()) {
                    return true;
                }
            }
        }
    }
    false
}

static KWIN_VERSION: OnceLock<Option<(u32, u32, u32)>> = OnceLock::new();

/// The running KWin's version (`kwin_wayland --version`), probed once.
pub fn kwin_version() -> Option<(u32, u32, u32)> {
    *KWIN_VERSION.get_or_init(probe_kwin_version)
}

fn probe_kwin_version() -> Option<(u32, u32, u32)> {
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
    if !screencast_granted() {
        // Not fatal: upstream KWin only filters sandboxed clients, so the
        // grant matters only on builds that kept the desktop-file check.
        eprintln!(
            "[backend-kde] preflight: no desktop file grants this binary \
             zkde_screencast_unstable_v1 (X-KDE-Wayland-Interfaces); KWin builds that \
             check it (Debian/Kali 6.7.4) will refuse the virtual output. Packages \
             install /usr/share/applications/wayrdp.desktop for /usr/bin/wayrdp."
        );
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
    fn desktop_file_grant_matching() {
        let exe = Path::new("/usr/bin/wayrdp");
        let id = |p: &Path| Some(p.to_path_buf());
        let shipped = include_str!("../../../packaging/desktop/wayrdp.desktop");
        assert!(desktop_grants(shipped, exe, id));
        // A dev build elsewhere is not covered by the packaged file.
        assert!(!desktop_grants(
            shipped,
            Path::new("/home/u/target/release/wayrdp"),
            id
        ));
        let list = "[Desktop Entry]\nExec=\"/usr/bin/wayrdp\" --x\n\
                    X-KDE-Wayland-Interfaces=org_kde_kwin_fake_input,zkde_screencast_unstable_v1\n";
        assert!(desktop_grants(list, exe, id));
        let missing = "[Desktop Entry]\nExec=/usr/bin/wayrdp\n";
        assert!(!desktop_grants(missing, exe, id));
        let other_group = "[Desktop Entry]\nExec=/usr/bin/wayrdp\n\
                           [Desktop Action x]\nX-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1\n";
        assert!(!desktop_grants(other_group, exe, id));
        // A relative Exec never canonicalizes to the binary in KWin either.
        let relative =
            "[Desktop Entry]\nExec=wayrdp\nX-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1\n";
        assert!(!desktop_grants(relative, exe, |p| p
            .is_absolute()
            .then(|| p.to_path_buf())));
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
