//! When to restart plasmashell after the screen swap.
//!
//! Plasma 6.7 moves the desktop containment from the physical screen to
//! `Virtual-wayrdp` when the layout guard disables the physical output, and
//! the folder view does not survive the move: plasmashell logs
//! `FolderViewLayer.qml:42: TypeError: Cannot read property 'model' of null`,
//! the desktop shows no icons and its context menu has three entries. A
//! plasmashell restart rebuilds it. Plasma 6.3 (Parrot) is unaffected, so
//! the default restarts only on KWin ≥ 6.7.
//!
//! `WAYRDP_KDE_RESTART_SHELL=auto|always|never` overrides the choice.

/// First KWin whose folder view breaks on the containment move.
pub const BROKEN_FROM: (u32, u32) = (6, 7);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Auto,
    Always,
    Never,
}

impl Policy {
    /// Parse `WAYRDP_KDE_RESTART_SHELL`; unset or unknown is `Auto`
    /// (unknown also returns the value so the caller can warn).
    pub fn parse(value: Option<&str>) -> (Policy, Option<String>) {
        let Some(v) = value else {
            return (Policy::Auto, None);
        };
        match v.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => (Policy::Auto, None),
            "always" | "1" | "true" | "yes" => (Policy::Always, None),
            "never" | "0" | "false" | "no" => (Policy::Never, None),
            _ => (Policy::Auto, Some(v.to_string())),
        }
    }

    pub fn from_env() -> Policy {
        let raw = std::env::var("WAYRDP_KDE_RESTART_SHELL").ok();
        let (policy, unknown) = Policy::parse(raw.as_deref());
        if let Some(v) = unknown {
            eprintln!(
                "[backend-kde] WAYRDP_KDE_RESTART_SHELL={v:?} not understood \
                 (auto|always|never); using auto"
            );
        }
        policy
    }

    /// Restart plasmashell once the swap has settled? An unknown KWin
    /// version restarts nothing under `Auto`: a needless restart blanks the
    /// desktop for a second or two, a missing one is fixable by hand.
    pub fn restart(self, kwin: Option<(u32, u32, u32)>) -> bool {
        match self {
            Policy::Always => true,
            Policy::Never => false,
            Policy::Auto => kwin.is_some_and(|(maj, min, _)| (maj, min) >= BROKEN_FROM),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_restarts_only_on_plasma_6_7_and_later() {
        let auto = Policy::Auto;
        assert!(!auto.restart(Some((6, 3, 6)))); // Parrot
        assert!(!auto.restart(Some((6, 6, 9))));
        assert!(auto.restart(Some((6, 7, 4)))); // Kali
        assert!(auto.restart(Some((7, 0, 0))));
        assert!(!auto.restart(None));
        assert!(Policy::Always.restart(Some((6, 3, 6))));
        assert!(!Policy::Never.restart(Some((6, 7, 4))));
    }

    #[test]
    fn env_values_parse() {
        assert_eq!(Policy::parse(None), (Policy::Auto, None));
        assert_eq!(Policy::parse(Some("auto")), (Policy::Auto, None));
        assert_eq!(Policy::parse(Some("Always")), (Policy::Always, None));
        assert_eq!(Policy::parse(Some("no")), (Policy::Never, None));
        assert_eq!(
            Policy::parse(Some("sometimes")),
            (Policy::Auto, Some("sometimes".into()))
        );
    }
}
