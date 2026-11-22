//! Startup checks for the GNOME backend, run once before the listener opens.
//!
//! Version policy (decided 2026-11-22): refuse GNOME older than the oldest
//! tested release, warn above the newest tested. Mutter's remote-desktop API
//! is private and may change between releases, so "untested" is a real risk;
//! `WAYRDP_GNOME_ALLOW_UNTESTED=1` overrides the refusal for validation runs.
//!
//! Permanent failures exit 42 (systemd stops retrying); transient ones (the
//! shell not on the bus yet at login) exit 1 so the unit retries.

use std::time::Duration;

use zbus::blocking::Connection;
use zbus::zvariant::OwnedValue;

use crate::mutter::{DC, RD, SC};

/// Oldest GNOME the backend is validated against (Kali, GNOME 50.5).
pub const MIN_TESTED: u32 = 50;
/// Newest GNOME the backend is validated against.
pub const NEWEST_TESTED: u32 = 50;

#[derive(Debug)]
pub enum Failure {
    Permanent(String),
    Transient(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Permanent(s) | Failure::Transient(s) => f.write_str(s),
        }
    }
}

/// Major version from `ShellVersion` ("50.5", "46.0", "47.rc").
pub fn parse_major(version: &str) -> Option<u32> {
    version.split('.').next()?.trim().parse().ok()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Tested,
    TooOld,
    Newer,
}

pub fn verdict(major: u32) -> Verdict {
    if major < MIN_TESTED {
        Verdict::TooOld
    } else if major > NEWEST_TESTED {
        Verdict::Newer
    } else {
        Verdict::Tested
    }
}

fn allow_untested() -> bool {
    matches!(
        std::env::var("WAYRDP_GNOME_ALLOW_UNTESTED").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn has_owner(conn: &Connection, name: &str) -> bool {
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &(name,),
    )
    .ok()
    .and_then(|r| r.body().deserialize::<bool>().ok())
    .unwrap_or(false)
}

fn shell_version(conn: &Connection) -> Option<String> {
    let reply = conn
        .call_method(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.gnome.Shell", "ShellVersion"),
        )
        .ok()?;
    let (v,): (OwnedValue,) = reply.body().deserialize().ok()?;
    String::try_from(v).ok()
}

pub fn preflight() -> Result<(), Failure> {
    if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("x11") {
        return Err(Failure::Permanent(
            "GNOME on X11: the gnome backend needs a GNOME Wayland session".into(),
        ));
    }
    let conn =
        Connection::session().map_err(|e| Failure::Transient(format!("no session bus: {e}")))?;
    // gnome-shell owns all three; at login it may not be up yet.
    let mut waited = Duration::ZERO;
    while !has_owner(&conn, "org.gnome.Shell") {
        if waited >= Duration::from_secs(10) {
            return Err(Failure::Transient(
                "gnome-shell is not on the session bus yet".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
        waited += Duration::from_millis(500);
    }
    for name in [RD, SC, DC] {
        if !has_owner(&conn, name) {
            return Err(Failure::Permanent(format!(
                "{name} is not on the session bus (Mutter built without remote desktop?)"
            )));
        }
    }
    let version = shell_version(&conn).unwrap_or_default();
    let Some(major) = parse_major(&version) else {
        eprintln!("[backend-gnome] preflight: GNOME version unknown ({version:?}); continuing");
        return Ok(());
    };
    match verdict(major) {
        Verdict::Tested => {
            eprintln!("[backend-gnome] preflight: GNOME {version}");
            Ok(())
        }
        Verdict::Newer => {
            eprintln!(
                "[backend-gnome] preflight: GNOME {version} is newer than the newest tested \
                 ({NEWEST_TESTED}); Mutter's remote-desktop API is private, so watch for breakage"
            );
            Ok(())
        }
        Verdict::TooOld if allow_untested() => {
            eprintln!(
                "[backend-gnome] preflight: GNOME {version} is older than the oldest tested \
                 ({MIN_TESTED}); continuing because WAYRDP_GNOME_ALLOW_UNTESTED is set"
            );
            Ok(())
        }
        Verdict::TooOld => Err(Failure::Permanent(format!(
            "GNOME {version} is older than the oldest tested release ({MIN_TESTED}); \
             set WAYRDP_GNOME_ALLOW_UNTESTED=1 to try anyway"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        assert_eq!(parse_major("50.5"), Some(50));
        assert_eq!(parse_major("46.0"), Some(46));
        assert_eq!(parse_major("47.rc"), Some(47));
        assert_eq!(parse_major(""), None);
        assert_eq!(parse_major("x.1"), None);
    }

    #[test]
    fn policy_refuses_old_and_warns_on_new() {
        assert_eq!(verdict(MIN_TESTED), Verdict::Tested);
        assert_eq!(verdict(MIN_TESTED - 1), Verdict::TooOld);
        assert_eq!(verdict(NEWEST_TESTED + 1), Verdict::Newer);
    }
}
