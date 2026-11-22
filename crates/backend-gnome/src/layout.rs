//! The layout guard, as pure decisions over Mutter's DisplayConfig state.
//!
//! While a session streams, the only enabled monitor is our virtual one, so
//! the client sees the whole desktop (panel, overview) and nothing renders
//! on the Hyper-V console's `Virtual-1`. Mutter restores the physical
//! monitor by itself when the session stops, and also when our process dies
//! (spike S1, Mutter 50.5), so teardown and startup only verify that and
//! repair a layout that did not come back.
//!
//! Configs are applied with the *temporary* method: nothing is written to
//! `monitors.xml` and no "keep these settings?" dialog appears.

/// One monitor from `GetCurrentState`.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub connector: String,
    pub current_mode: Option<String>,
    pub preferred_mode: Option<String>,
    /// Size of the current mode, if any.
    pub current_size: Option<(u32, u32)>,
}

/// One enabled logical monitor from `GetCurrentState`.
#[derive(Debug, Clone, PartialEq)]
pub struct Logical {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub connectors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub serial: u32,
    pub monitors: Vec<Monitor>,
    pub logical: Vec<Logical>,
}

/// A logical monitor to apply: position, scale, primary, and the
/// (connector, mode id) pairs it shows.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalConfig {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub primary: bool,
    pub monitors: Vec<(String, String)>,
}

impl State {
    pub fn monitor(&self, connector: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.connector == connector)
    }

    pub fn enabled(&self, connector: &str) -> bool {
        self.logical
            .iter()
            .any(|l| l.connectors.iter().any(|c| c == connector))
    }

    pub fn connectors(&self) -> Vec<String> {
        self.monitors.iter().map(|m| m.connector.clone()).collect()
    }
}

/// Our virtual monitor: a connector that was not there before
/// `RecordVirtual` (Mutter names them `Meta-N`), preferring one already at
/// the size we asked for.
pub fn find_ours<'a>(before: &[String], now: &'a State, size: (u32, u32)) -> Option<&'a Monitor> {
    let fresh = || {
        now.monitors
            .iter()
            .filter(|m| !before.contains(&m.connector))
    };
    fresh()
        .find(|m| m.current_size == Some(size))
        .or_else(|| fresh().next())
}

/// Whether the guard holds: ours is enabled and nothing else is.
pub fn guarded(state: &State, ours: &str) -> bool {
    state.enabled(ours)
        && state
            .logical
            .iter()
            .all(|l| l.connectors.iter().all(|c| c == ours))
}

/// The guard layout: only our monitor, at the origin, primary, scale
/// `scale`, in its current mode. `None` while Mutter has no mode for it yet
/// (the stream has not negotiated a size).
pub fn guard_config(state: &State, ours: &str, scale: f64) -> Option<Vec<LogicalConfig>> {
    let mode = state.monitor(ours)?.current_mode.clone()?;
    Some(vec![LogicalConfig {
        x: 0,
        y: 0,
        scale,
        primary: true,
        monitors: vec![(ours.to_string(), mode)],
    }])
}

/// Physical monitors: every connector that is not a Mutter virtual monitor.
pub fn physical(state: &State) -> impl Iterator<Item = &Monitor> {
    state
        .monitors
        .iter()
        .filter(|m| !m.connector.starts_with("Meta-"))
}

/// Whether some physical monitor is enabled (the console is usable).
pub fn physical_enabled(state: &State) -> bool {
    physical(state).any(|m| state.enabled(&m.connector))
}

/// A layout that brings every physical monitor back, side by side at its
/// preferred mode, the first one primary. Used only when Mutter did not
/// restore the console itself. `None` when there is nothing to enable.
pub fn restore_config(state: &State) -> Option<Vec<LogicalConfig>> {
    let mut x = 0i32;
    let mut out = Vec::new();
    for m in physical(state) {
        let Some(mode) = m.preferred_mode.clone().or_else(|| m.current_mode.clone()) else {
            continue;
        };
        let width = mode_width(&mode).unwrap_or(1024);
        out.push(LogicalConfig {
            x,
            y: 0,
            scale: 1.0,
            primary: out.is_empty(),
            monitors: vec![(m.connector.clone(), mode)],
        });
        x += width as i32;
    }
    (!out.is_empty()).then_some(out)
}

/// Width from a Mutter mode id such as `1024x768@60.004`.
fn mode_width(mode: &str) -> Option<u32> {
    mode.split('x').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(c: &str, cur: Option<&str>, size: Option<(u32, u32)>) -> Monitor {
        Monitor {
            connector: c.into(),
            current_mode: cur.map(Into::into),
            preferred_mode: cur.map(Into::into),
            current_size: size,
        }
    }

    fn logical(x: i32, cs: &[&str]) -> Logical {
        Logical {
            x,
            y: 0,
            scale: 1.0,
            connectors: cs.iter().map(|c| c.to_string()).collect(),
        }
    }

    /// The S1 layout on Kali: `Virtual-1` plus our 1280x800 `Meta-0`.
    fn s1() -> State {
        State {
            serial: 8,
            monitors: vec![
                mon("Virtual-1", Some("1024x768@60.004"), Some((1024, 768))),
                mon("Meta-0", Some("1280x800@60.000"), Some((1280, 800))),
            ],
            logical: vec![logical(0, &["Virtual-1"]), logical(1024, &["Meta-0"])],
        }
    }

    #[test]
    fn ours_is_the_new_connector() {
        let before = vec!["Virtual-1".to_string()];
        assert_eq!(
            find_ours(&before, &s1(), (1280, 800)).map(|m| m.connector.as_str()),
            Some("Meta-0")
        );
        // A stale Meta-0 from another client: prefer the one at our size.
        let mut st = s1();
        st.monitors
            .push(mon("Meta-1", Some("1920x1080@60.000"), Some((1920, 1080))));
        assert_eq!(
            find_ours(&before, &st, (1920, 1080)).map(|m| m.connector.as_str()),
            Some("Meta-1")
        );
        assert!(find_ours(&st.connectors(), &st, (1280, 800)).is_none());
    }

    #[test]
    fn guard_enables_only_ours() {
        let st = s1();
        assert!(!guarded(&st, "Meta-0"));
        let cfg = guard_config(&st, "Meta-0", 1.0).unwrap();
        assert_eq!(
            cfg,
            vec![LogicalConfig {
                x: 0,
                y: 0,
                scale: 1.0,
                primary: true,
                monitors: vec![("Meta-0".into(), "1280x800@60.000".into())],
            }]
        );
        let mut held = st.clone();
        held.logical = vec![logical(0, &["Meta-0"])];
        assert!(guarded(&held, "Meta-0"));
        assert!(!physical_enabled(&held));
    }

    #[test]
    fn no_guard_before_the_mode_exists() {
        let mut st = s1();
        st.monitors[1].current_mode = None;
        assert!(guard_config(&st, "Meta-0", 1.0).is_none());
    }

    #[test]
    fn restore_brings_physical_monitors_back() {
        let mut st = s1();
        st.logical = vec![logical(0, &["Meta-0"])];
        let cfg = restore_config(&st).unwrap();
        assert_eq!(cfg.len(), 1);
        assert_eq!(
            cfg[0].monitors,
            vec![("Virtual-1".into(), "1024x768@60.004".into())]
        );
        assert!(cfg[0].primary);
        assert!(physical_enabled(&s1()));
    }

    #[test]
    fn two_physical_monitors_restore_side_by_side() {
        let st = State {
            serial: 1,
            monitors: vec![
                mon("Virtual-1", Some("1024x768@60.004"), Some((1024, 768))),
                mon("Virtual-2", Some("800x600@60.000"), Some((800, 600))),
            ],
            logical: vec![],
        };
        let cfg = restore_config(&st).unwrap();
        assert_eq!((cfg[0].x, cfg[1].x), (0, 1024));
        assert!(cfg[0].primary && !cfg[1].primary);
    }
}
