//! Resize coalescing and size sanitizing, pure so the rules are unit-tested.
//!
//! vmconnect and xfreerdp `/dynamic-resolution` send bursts while the user
//! drags, and rdp-core's drift correction re-sends the SAME size every 30
//! mismatched frames during a transition. A non-idempotent resize here
//! would become a KWin output-recreate storm, so: wait for a quiet period,
//! apply only the newest size, and treat repeats as no-ops.

use std::time::{Duration, Instant};

use desk_backend::Size;

/// Quiet period after the last size change before it is applied.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

pub const MIN: Size = Size {
    width: 640,
    height: 480,
};
pub const MAX: Size = Size {
    width: 8192,
    height: 8192,
};

/// Clamp into KWin-sane bounds and round down to even dimensions (encoders
/// and KWin custom modes want even sizes). Returns the size and whether it
/// had to be clamped.
pub fn sanitize(size: Size) -> (Size, bool) {
    let w = size.width.clamp(MIN.width, MAX.width);
    let h = size.height.clamp(MIN.height, MAX.height);
    let clamped = w != size.width || h != size.height;
    (
        Size {
            width: w & !1,
            height: h & !1,
        },
        clamped,
    )
}

#[derive(Debug)]
pub struct Coalescer {
    current: Size,
    pending: Option<(Size, Instant)>,
}

impl Coalescer {
    pub fn new(current: Size) -> Self {
        Self {
            current,
            pending: None,
        }
    }

    pub fn current(&self) -> Size {
        self.current
    }

    /// Record a (sanitized) request.
    pub fn request(&mut self, size: Size, at: Instant) {
        match self.pending {
            // A repeat of the pending size must not postpone it.
            Some((p, _)) if p == size => {}
            // Dragged back to where we are: nothing to do.
            _ if size == self.current => self.pending = None,
            _ => self.pending = Some((size, at)),
        }
    }

    /// When the pending request becomes due, if any.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, at)| at + DEBOUNCE)
    }

    /// The size to apply now, if the pending request has been quiet long
    /// enough. The caller reports the result through [`Self::applied`].
    pub fn due(&mut self, now: Instant) -> Option<Size> {
        let (size, at) = self.pending?;
        if now.saturating_duration_since(at) < DEBOUNCE {
            return None;
        }
        self.pending = None;
        Some(size)
    }

    pub fn applied(&mut self, size: Size) {
        self.current = size;
        if self.pending.is_some_and(|(p, _)| p == size) {
            self.pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sz(width: u32, height: u32) -> Size {
        Size { width, height }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn sanitize_clamps_and_evens() {
        assert_eq!(sanitize(sz(1366, 768)), (sz(1366, 768), false));
        assert_eq!(sanitize(sz(1367, 769)), (sz(1366, 768), false));
        assert_eq!(sanitize(sz(100, 100)), (sz(640, 480), true));
        assert_eq!(sanitize(sz(10_000, 9_001)), (sz(8192, 8192), true));
    }

    #[test]
    fn a_burst_collapses_to_the_last_size() {
        let t = Instant::now();
        let mut c = Coalescer::new(sz(1024, 768));
        c.request(sz(1100, 700), t);
        c.request(sz(1200, 750), t + ms(50));
        c.request(sz(1300, 800), t + ms(100));
        assert_eq!(c.due(t + ms(300)), None);
        assert_eq!(c.deadline(), Some(t + ms(350)));
        assert_eq!(c.due(t + ms(350)), Some(sz(1300, 800)));
        c.applied(sz(1300, 800));
        assert_eq!(c.due(t + ms(2_000)), None);
        assert_eq!(c.current(), sz(1300, 800));
    }

    #[test]
    fn repeats_of_the_current_size_are_no_ops() {
        let t = Instant::now();
        let mut c = Coalescer::new(sz(1920, 1080));
        for i in 0..10 {
            c.request(sz(1920, 1080), t + ms(i * 100));
        }
        assert_eq!(c.deadline(), None);
        assert_eq!(c.due(t + ms(5_000)), None);
    }

    #[test]
    fn repeats_of_the_pending_size_do_not_postpone_it() {
        let t = Instant::now();
        let mut c = Coalescer::new(sz(1024, 768));
        c.request(sz(1366, 768), t);
        c.request(sz(1366, 768), t + ms(200));
        assert_eq!(c.due(t + ms(250)), Some(sz(1366, 768)));
    }

    #[test]
    fn dragging_back_to_the_current_size_cancels() {
        let t = Instant::now();
        let mut c = Coalescer::new(sz(1024, 768));
        c.request(sz(1366, 768), t);
        c.request(sz(1024, 768), t + ms(100));
        assert_eq!(c.due(t + ms(1_000)), None);
    }
}
