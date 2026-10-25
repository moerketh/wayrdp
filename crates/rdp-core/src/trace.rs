//! Per-peer frame pipeline trace (WAYRDP_TRACE=1): where a frame's time goes
//! between the capture plugin's publish and the client's FrameAcknowledge.
//!
//! Stages, all CLOCK_MONOTONIC milliseconds (u32, wrapping):
//! - `cap→deq`: plugin publish stamp → the mainloop dequeues the frame
//!   (pump copy-out + channel + mainloop wake);
//! - `enc`: RemoteFX encode of the frame;
//! - `send→ack`: SurfaceFrameCommand queued → FrameAcknowledge for the same
//!   frameId (wire + client decode + client ack batching);
//! - `inflight`: frames sent but not yet acknowledged, sampled at each send;
//! - `qdepth`: the client's own queueDepth from the acks (frames it holds
//!   undecoded; MS-RDPEGFX 2.2.3.10).
//!
//! The input side (input → next published frame) lives in the capture pump,
//! which sees both. Everything here runs on the peer's mainloop thread; the
//! trace is diagnostics only and never changes what gets sent.

use desk_backend::stats::LatStats;

/// Seconds between `[trace]` lines.
const WINDOW_SECS: u64 = 5;

/// Send stamps kept for ack matching. An ack older than this many frames is
/// counted as unmatched instead of producing a bogus sample.
const RING: usize = 512;

/// Acknowledgements that stop arriving (SUSPEND_FRAME_ACKNOWLEDGEMENT) are
/// signalled with this queueDepth; it is not a depth.
const SUSPEND_ACKS: u32 = 0xFFFF_FFFF;

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WAYRDP_TRACE").is_some())
}

pub struct FrameTrace {
    window_start_ms: u32,
    cap_to_deq: LatStats,
    encode: LatStats,
    send_to_ack: LatStats,
    inflight: LatStats,
    queue_depth: LatStats,
    sent: u64,
    /// Frames dequeued but replaced by a newer one before encode
    /// (latest-wins drain).
    superseded: u64,
    /// Send attempts held back by the in-flight window.
    gated: u64,
    unmatched_acks: u64,
    /// (frameId, send stamp) by frameId % RING.
    sent_at: Vec<(u32, u32)>,
}

impl FrameTrace {
    pub fn new(now_ms: u32) -> Self {
        Self {
            window_start_ms: now_ms,
            cap_to_deq: LatStats::default(),
            encode: LatStats::default(),
            send_to_ack: LatStats::default(),
            inflight: LatStats::default(),
            queue_depth: LatStats::default(),
            sent: 0,
            superseded: 0,
            gated: 0,
            unmatched_acks: 0,
            // frameId 0 is never sent (ids start at 1), so (0, _) = empty.
            sent_at: vec![(0, 0); RING],
        }
    }

    /// A frame left the channel for encoding; `superseded` older frames were
    /// drained and dropped with it. `capture_stamp_ms` 0 = unstamped.
    pub fn on_dequeue(&mut self, capture_stamp_ms: u32, superseded: u64, now_ms: u32) {
        self.superseded += superseded;
        if capture_stamp_ms != 0 {
            self.cap_to_deq.push(now_ms.wrapping_sub(capture_stamp_ms));
        }
    }

    pub fn on_gated(&mut self) {
        self.gated += 1;
    }

    pub fn on_sent(&mut self, frame_id: u32, encode_ms: u32, inflight: u32, now_ms: u32) {
        self.sent += 1;
        self.encode.push(encode_ms);
        self.inflight.push(inflight);
        self.sent_at[frame_id as usize % RING] = (frame_id, now_ms);
    }

    pub fn on_ack(&mut self, frame_id: u32, queue_depth: u32, now_ms: u32) {
        if queue_depth != SUSPEND_ACKS {
            self.queue_depth.push(queue_depth);
        }
        let slot = &mut self.sent_at[frame_id as usize % RING];
        if frame_id != 0 && slot.0 == frame_id {
            self.send_to_ack.push(now_ms.wrapping_sub(slot.1));
            // One ack per frame: a duplicate must not count again.
            *slot = (0, 0);
        } else {
            self.unmatched_acks += 1;
        }
    }

    /// The `[trace]` line once a window has elapsed (and a new window starts),
    /// else None.
    pub fn take_line(&mut self, now_ms: u32) -> Option<String> {
        let elapsed = now_ms.wrapping_sub(self.window_start_ms);
        if (elapsed as u64) < WINDOW_SECS * 1000 {
            return None;
        }
        let fps = self.sent as f64 * 1000.0 / elapsed as f64;
        let ack = self.send_to_ack.take_summary();
        let line = format!(
            "[trace] {:.1}s sent={} fps={:.1} superseded={} gated={} \
             cap→deq={} enc={} send→ack={} acks={} unmatched={} inflight={} qdepth={}",
            elapsed as f64 / 1000.0,
            self.sent,
            fps,
            self.superseded,
            self.gated,
            self.cap_to_deq.take_summary(),
            self.encode.take_summary(),
            ack,
            ack.n,
            self.unmatched_acks,
            self.inflight.take_summary(),
            self.queue_depth.take_summary(),
        );
        self.window_start_ms = now_ms;
        self.sent = 0;
        self.superseded = 0;
        self.gated = 0;
        self.unmatched_acks = 0;
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_matches_its_own_send() {
        let mut t = FrameTrace::new(0);
        t.on_sent(7, 9, 1, 100);
        t.on_sent(8, 9, 2, 116);
        t.on_ack(7, 1, 140);
        t.on_ack(8, 0, 170);
        let line = t.take_line(5000).unwrap();
        // 40 and 54 ms; p50 of two samples is the larger one.
        assert!(
            line.contains("send→ack=54/54/54 acks=2 unmatched=0"),
            "{line}"
        );
    }

    #[test]
    fn duplicate_or_unknown_acks_are_unmatched() {
        let mut t = FrameTrace::new(0);
        t.on_sent(3, 5, 1, 10);
        t.on_ack(3, 0, 30);
        t.on_ack(3, 0, 31);
        t.on_ack(99, 0, 32);
        let line = t.take_line(5000).unwrap();
        assert!(line.contains("acks=1 unmatched=2"), "{line}");
    }

    #[test]
    fn ring_reuse_does_not_match_a_stale_frame() {
        let mut t = FrameTrace::new(0);
        t.on_sent(1, 5, 1, 10);
        t.on_sent(1 + RING as u32, 5, 1, 20);
        t.on_ack(1, 0, 30);
        let line = t.take_line(5000).unwrap();
        assert!(line.contains("acks=0 unmatched=1"), "{line}");
    }

    #[test]
    fn suspend_marker_is_not_a_queue_depth() {
        let mut t = FrameTrace::new(0);
        t.on_sent(1, 5, 1, 10);
        t.on_ack(1, SUSPEND_ACKS, 20);
        let line = t.take_line(5000).unwrap();
        assert!(line.ends_with("qdepth=0/0/0"), "{line}");
    }

    #[test]
    fn line_only_after_the_window_and_across_wrap() {
        let start = u32::MAX - 1000;
        let mut t = FrameTrace::new(start);
        t.on_dequeue(start.wrapping_sub(3), 2, start);
        assert!(t.take_line(start.wrapping_add(4999)).is_none());
        let line = t.take_line(start.wrapping_add(5000)).unwrap();
        assert!(line.contains("superseded=2"), "{line}");
        assert!(line.contains("cap→deq=3/3/3"), "{line}");
        // New window: counters restart.
        let line = t.take_line(start.wrapping_add(10000)).unwrap();
        assert!(line.contains("superseded=0"), "{line}");
    }
}
