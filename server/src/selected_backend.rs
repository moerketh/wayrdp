//! Exit-code plumbing: unsupported desktops (KDE, Sway) bail out with this
//! dedicated status so the systemd unit can mark it
//! `RestartPreventExitStatus=` instead of restarting every 5s forever.

/// Exit status for "there is no working backend on this desktop, fix the
/// configuration" — i.e. restart loops won't help. 42 is unused by both
/// systemd's well-known codes (like 78/EX_CONFIG are) and anything else we
/// call.
pub const EXIT_NO_BACKEND: i32 = 42;

pub trait TaggedError: Sized {
    fn apply(self, err: anyhow::Error) -> anyhow::Error;
    fn of(err: &anyhow::Error) -> Option<i32>;
}

impl TaggedError for i32 {
    fn apply(self, err: anyhow::Error) -> anyhow::Error {
        // The tag must stay the outermost context: exit_code() checks only
        // that layer, because anyhow doesn't flatten nested chains for
        // downcasting. Bail sites apply the tag last to guarantee this.
        err.context(ExitTag(self))
    }
    fn of(err: &anyhow::Error) -> Option<i32> {
        err.downcast_ref::<ExitTag>().map(|t| t.0)
    }
}

#[derive(Debug)]
struct ExitTag(i32);

impl std::fmt::Display for ExitTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(exit status {})", self.0)
    }
}

impl std::error::Error for ExitTag {}

/// Exit status to use for this error, defaulting to 1 like anyhow's
/// `main() -> Result` termination.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    i32::of(err).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untagged_error_exits_1() {
        assert_eq!(exit_code(&anyhow::anyhow!("boom")), 1);
    }

    #[test]
    fn tagged_error_exits_tag() {
        let e = EXIT_NO_BACKEND.apply(anyhow::anyhow!("no backend"));
        assert_eq!(exit_code(&e), EXIT_NO_BACKEND);
        // tag message stays out of the way of the real error text
        assert!(format!("{e:#}").contains("no backend"));
    }
}
