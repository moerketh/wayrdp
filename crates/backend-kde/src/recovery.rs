//! Startup console recovery (WP7).
//!
//! A SIGKILL/OOM/abort of wayrdp while a session is live leaves the layout
//! guard's work undone: the physical outputs stay disabled, and the machine
//! is headless once the user disconnects (Hyper-V's basic console shows
//! nothing). On daemon start, before any connection is accepted, re-enable
//! them. wayrdp is the only thing that disables them on these guests; a
//! user who runs headless on purpose sets `WAYRDP_KDE_RESTORE_ON_START=0`.

use std::process::{Command, Stdio};
use std::time::Duration;

use hyperv_rdp_extras::session::{
    enable_output, parse_enabled_physical_outputs, parse_recoverable_physical_outputs, strip_ansi,
};

/// Our virtual output's kscreen name (`VirtualOutputConfig::new("wayrdp")`).
pub const KSCREEN_NAME: &str = "Virtual-wayrdp";

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// At least one physical output is enabled: nothing to do.
    Healthy,
    /// No physical output enabled: re-enable these.
    Restore(Vec<String>),
    /// No physical output enabled and none known (kscreen said nothing
    /// usable): leave it, log.
    NothingToRestore,
}

/// Pure decision over `kscreen-doctor -o` text.
pub fn plan(kscreen_text: &str) -> Plan {
    let text = strip_ansi(kscreen_text);
    if !parse_enabled_physical_outputs(&text, KSCREEN_NAME).is_empty() {
        return Plan::Healthy;
    }
    let recoverable = parse_recoverable_physical_outputs(&text, KSCREEN_NAME);
    if recoverable.is_empty() {
        Plan::NothingToRestore
    } else {
        Plan::Restore(recoverable)
    }
}

fn restore_enabled() -> bool {
    !matches!(
        std::env::var("WAYRDP_KDE_RESTORE_ON_START").as_deref(),
        Ok("0") | Ok("false") | Ok("no")
    )
}

/// `kscreen-doctor -o`, bounded: a wedged KWin must not hang startup.
fn kscreen_outputs(timeout: Duration) -> Option<String> {
    let mut child = Command::new("kscreen-doctor")
        .arg("-o")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run at daemon start when the KDE backend is selected. Blocking (≤ ~6 s);
/// never fails startup.
pub fn recover_at_startup() {
    if !restore_enabled() {
        return;
    }
    let Some(text) = kscreen_outputs(Duration::from_secs(5)) else {
        eprintln!(
            "[backend-kde] startup recovery: kscreen-doctor -o failed or timed out; skipping"
        );
        return;
    };
    if strip_ansi(&text).lines().any(|l| l.contains(KSCREEN_NAME)) {
        // Owned by the dead process; KWin destroys it with that client.
        eprintln!("[backend-kde] startup recovery: a stale {KSCREEN_NAME} output is still listed");
    }
    match plan(&text) {
        Plan::Healthy => {}
        Plan::NothingToRestore => {
            eprintln!("[backend-kde] startup recovery: no enabled or recoverable physical output");
        }
        Plan::Restore(outputs) => {
            for name in outputs {
                let ok = enable_output(&name);
                eprintln!(
                    "[backend-kde] startup recovery: physical output {name} was left disabled \
                     (previous wayrdp did not exit cleanly); re-enabled: {ok}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADLESS: &str = "\
Output: 1 Virtual-1
\tdisabled
\tconnected
\tpriority 0
\tModes: 1:1024x768@60*!
\tGeometry: 0,0 1024x768
Output: 2 Virtual-wayrdp
\tenabled
\tconnected
\tGeometry: 0,0 1366x768
";

    const HEALTHY: &str = "\
Output: 1 Virtual-1 4c0e7c54-1111-2222-3333-444455556666
\tenabled
\tconnected
\tGeometry: 0,0 1024x768
";

    #[test]
    fn healthy_console_is_left_alone() {
        assert_eq!(plan(HEALTHY), Plan::Healthy);
    }

    #[test]
    fn disabled_physical_output_is_restored() {
        assert_eq!(plan(HEADLESS), Plan::Restore(vec!["Virtual-1".into()]));
    }

    #[test]
    fn ansi_colours_do_not_hide_outputs() {
        let coloured = HEADLESS.replace("disabled", "\u{1b}[31mdisabled\u{1b}[0m");
        assert_eq!(plan(&coloured), Plan::Restore(vec!["Virtual-1".into()]));
    }

    #[test]
    fn empty_listing_restores_nothing() {
        assert_eq!(plan(""), Plan::NothingToRestore);
    }
}
