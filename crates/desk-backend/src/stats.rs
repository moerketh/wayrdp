//! Windowed latency statistics shared by the capture pumps and the RDP peer
//! (WAYRDP_DEBUG_CAPTURE / WAYRDP_TRACE logging). Samples are u32
//! milliseconds; `take_summary` reports the window and starts a new one.

/// Keeps at most this many samples per window for the percentiles; count,
/// average and max still cover every sample.
const MAX_SAMPLES: usize = 4096;

#[derive(Default)]
pub struct LatStats {
    n: u64,
    sum: u64,
    max: u32,
    samples: Vec<u32>,
}

/// One window's figures; all zero when the window had no samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatSummary {
    pub n: u64,
    pub avg: u32,
    pub p50: u32,
    pub p95: u32,
    pub max: u32,
}

impl LatStats {
    pub fn push(&mut self, ms: u32) {
        self.n += 1;
        self.sum += ms as u64;
        self.max = self.max.max(ms);
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(ms);
        }
    }

    /// Summarises and clears the window.
    pub fn take_summary(&mut self) -> LatSummary {
        let n = self.n;
        let avg = self.sum.checked_div(n).unwrap_or(0) as u32;
        let mut v = std::mem::take(&mut self.samples);
        v.sort_unstable();
        let pick = |q: usize| {
            if v.is_empty() {
                0
            } else {
                v[(v.len() * q / 100).min(v.len() - 1)]
            }
        };
        let s = LatSummary {
            n,
            avg,
            p50: pick(50),
            p95: pick(95),
            max: self.max,
        };
        self.n = 0;
        self.sum = 0;
        self.max = 0;
        s
    }
}

impl std::fmt::Display for LatSummary {
    /// `p50/p95/max`, the compact form the trace lines use.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.p50, self.p95, self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_window_is_all_zero() {
        assert_eq!(LatStats::default().take_summary(), LatSummary::default());
    }

    #[test]
    fn percentiles_and_reset() {
        let mut s = LatStats::default();
        for ms in 1..=100 {
            s.push(ms);
        }
        let w = s.take_summary();
        assert_eq!((w.n, w.avg, w.p50, w.p95, w.max), (100, 50, 51, 96, 100));
        assert_eq!(w.to_string(), "51/96/100");
        assert_eq!(s.take_summary().n, 0);
    }

    #[test]
    fn sample_cap_keeps_count_and_max_exact() {
        let mut s = LatStats::default();
        for _ in 0..(MAX_SAMPLES + 10) {
            s.push(1);
        }
        s.push(500);
        let w = s.take_summary();
        assert_eq!(w.n, MAX_SAMPLES as u64 + 11);
        assert_eq!(w.max, 500);
    }
}
