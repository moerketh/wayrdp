//! Connect-time health monitor: decides WHEN to heal the desktop layout or
//! recreate the virtual output. The heal MECHANICS belong to each backend
//! (KDE: extras' `heal_output_layout`, escalation = plasmashell restart;
//! GNOME: re-apply the layout guard, no escalation lever). This module only
//! turns observations into actions, as a pure state machine so every rule is
//! unit-tested without a compositor.
//!
//! Signals, computed by the capture thread from a sparse pixel sample (never
//! whole frames):
//! - a **uniform** frame (every sampled pixel identical). All-zero/alpha-0 is
//!   the "virtual output not rendered into" signature (output disabled, or
//!   plasmashell on its placeholder screen); a uniform non-zero colour is a
//!   desktop without its containment (no wallpaper, no panel);
//! - **no frame** since the stream was (re)connected.
//!
//! KWin only sends frames on damage, so an idle desktop is silent and that
//! is healthy. A uniform frame keeps counting while no newer frame arrives:
//! the streak is "the last frame seen was uniform, since T".
//!
//! Conservative by design: "zero heals on a healthy layout" is a Phase 1
//! gate. A single uniform frame (a fullscreen black terminal) never heals;
//! only a uniform streak inside the connect window does, and escalation
//! (a plasmashell restart, disruptive) needs stronger evidence.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// No first frame this long after a (re)connect → recreate the output once.
pub const NO_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// Uniform streaks only start heals inside this window after a (re)connect.
pub const CONNECT_WINDOW: Duration = Duration::from_secs(20);
/// A uniform streak this long (inside the window) triggers a surgical heal.
pub const UNIFORM_STREAK: Duration = Duration::from_secs(4);
/// Still uniform this long after a surgical heal → escalate.
pub const ESCALATE_AFTER: Duration = Duration::from_secs(8);
/// Still uniform this long after escalating → stop healing (log, keep
/// streaming).
pub const STOP_AFTER: Duration = Duration::from_secs(45);
/// Minimum spacing between two heals of any kind.
pub const HEAL_SPACING: Duration = Duration::from_secs(8);
/// Stream losses tolerated per [`LOSS_WINDOW`] before the session gives up.
pub const MAX_LOSSES: usize = 3;
pub const LOSS_WINDOW: Duration = Duration::from_secs(60);

/// What a uniform frame was uniform with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uniform {
    /// All-zero / alpha-0: nothing rendered into the output.
    Unrendered,
    /// One solid non-zero colour (a desktop without containment, or a solid
    /// wallpaper — the reason escalation needs more evidence).
    Solid,
}

/// Health sample attached to a delivered frame. Only some frames are
/// sampled (all inside the connect window, ~1/s after), and an unsampled
/// frame must not read as "real content": that would reset the uniform
/// streak between samples and a blank desktop would never be healed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    /// Not inspected; proves only that the stream is alive.
    Skipped,
    /// Inspected and not uniform: a rendered desktop.
    Content,
    Uniform(Uniform),
}

impl Sample {
    pub fn of(uniform: Option<Uniform>) -> Self {
        uniform.map_or(Sample::Content, Sample::Uniform)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    /// The capture stream was (re)connected: session start, recreate, or an
    /// applied resize. Re-arms the connect window.
    Connected(Instant),
    /// The layout guard finished engaging (physical outputs off, virtual
    /// output at the origin). Uniform frames before this are the expected
    /// "empty secondary screen" of the first second and never count.
    LayoutSettled(Instant),
    Frame {
        sample: Sample,
        at: Instant,
    },
    /// The client injected input (so a solid colour may be user content).
    InputInjected,
    StreamLost(Instant),
    Tick(Instant),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// Heal the layout. `escalate` asks for the backend's disruptive heal
    /// (KDE: plasmashell restart); a backend without one heals as usual.
    Heal {
        escalate: bool,
    },
    /// Recreate the virtual output (new node).
    Recreate,
    /// Healing exhausted: log loudly, keep streaming.
    StopHealing,
    /// End the session (rdp-core's watchdogs tear the peer down).
    GiveUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Watching,
    Healed(Instant),
    Escalated(Instant),
    Exhausted,
}

#[derive(Debug)]
pub struct Health {
    connected_at: Option<Instant>,
    settled_at: Option<Instant>,
    got_frame: bool,
    no_frame_recreated: bool,
    uniform_since: Option<(Instant, Uniform)>,
    stage: Stage,
    last_heal: Option<Instant>,
    escalated_once: bool,
    input_seen: bool,
    losses: VecDeque<Instant>,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl Health {
    pub fn new() -> Self {
        Self {
            connected_at: None,
            settled_at: None,
            got_frame: false,
            no_frame_recreated: false,
            uniform_since: None,
            stage: Stage::Watching,
            last_heal: None,
            escalated_once: false,
            input_seen: false,
            losses: VecDeque::new(),
        }
    }

    pub fn observe(&mut self, obs: Observation) -> Action {
        match obs {
            Observation::Connected(at) => {
                self.connected_at = Some(at);
                self.settled_at = None;
                self.got_frame = false;
                self.uniform_since = None;
                self.stage = Stage::Watching;
                Action::None
            }
            Observation::LayoutSettled(at) => {
                // A later settle point (planned plasmashell restart) also
                // voids a streak begun before it: those frames never count.
                if self.uniform_since.is_some_and(|(t, _)| t < at) {
                    self.uniform_since = None;
                }
                self.settled_at = Some(at);
                Action::None
            }
            Observation::Frame { sample, at } => {
                self.got_frame = true;
                // A frame proves the stream works; a later no-frame stall gets
                // its own recreate again.
                self.no_frame_recreated = false;
                match sample {
                    Sample::Skipped => {}
                    Sample::Content => {
                        self.uniform_since = None;
                        // A real frame means the layout healed (or never
                        // broke): reset the ladder, but escalation stays
                        // once per session.
                        if self.stage != Stage::Exhausted {
                            self.stage = Stage::Watching;
                        }
                    }
                    Sample::Uniform(kind) => {
                        if self.settled_at.is_some_and(|s| at >= s) {
                            match self.uniform_since {
                                Some((t, _)) => self.uniform_since = Some((t, kind)),
                                None => self.uniform_since = Some((at, kind)),
                            }
                        }
                    }
                }
                Action::None
            }
            Observation::InputInjected => {
                self.input_seen = true;
                Action::None
            }
            Observation::StreamLost(at) => {
                self.losses.push_back(at);
                while self
                    .losses
                    .front()
                    .is_some_and(|&t| at.saturating_duration_since(t) > LOSS_WINDOW)
                {
                    self.losses.pop_front();
                }
                if self.losses.len() > MAX_LOSSES {
                    Action::GiveUp
                } else {
                    Action::Recreate
                }
            }
            Observation::Tick(now) => self.tick(now),
        }
    }

    fn tick(&mut self, now: Instant) -> Action {
        let Some(connected) = self.connected_at else {
            return Action::None;
        };
        if !self.got_frame {
            if now.saturating_duration_since(connected) < NO_FRAME_TIMEOUT {
                return Action::None;
            }
            if self.no_frame_recreated {
                return Action::GiveUp;
            }
            self.no_frame_recreated = true;
            // The recreate reports Connected again; until then don't fire
            // twice.
            self.connected_at = Some(now);
            return Action::Recreate;
        }
        let Some((since, kind)) = self.uniform_since else {
            return Action::None;
        };
        let spaced = self
            .last_heal
            .is_none_or(|h| now.saturating_duration_since(h) >= HEAL_SPACING);
        match self.stage {
            Stage::Watching => {
                let in_window = since.saturating_duration_since(connected) < CONNECT_WINDOW;
                if in_window && now.saturating_duration_since(since) >= UNIFORM_STREAK && spaced {
                    self.stage = Stage::Healed(now);
                    self.last_heal = Some(now);
                    return Action::Heal { escalate: false };
                }
                Action::None
            }
            Stage::Healed(at) => {
                if now.saturating_duration_since(at) < ESCALATE_AFTER || !spaced {
                    return Action::None;
                }
                // A plasmashell restart is disruptive: escalate only once per
                // session, and only when the colour can't be user content (an
                // unrendered output, or nothing typed or clicked yet).
                if !self.escalated_once && (kind == Uniform::Unrendered || !self.input_seen) {
                    self.escalated_once = true;
                    self.stage = Stage::Escalated(now);
                    self.last_heal = Some(now);
                    return Action::Heal { escalate: true };
                }
                self.stage = Stage::Exhausted;
                Action::StopHealing
            }
            Stage::Escalated(at) => {
                if now.saturating_duration_since(at) >= STOP_AFTER {
                    self.stage = Stage::Exhausted;
                    return Action::StopHealing;
                }
                Action::None
            }
            Stage::Exhausted => Action::None,
        }
    }
}

/// Classify a frame from a sparse grid sample: `None` = real content.
/// `bytes` is BGRx/BGRA rows of `stride` bytes; only ~`GRID`² pixels are read.
pub fn sample_uniform(bytes: &[u8], width: u32, height: u32, stride: usize) -> Option<Uniform> {
    const GRID: u32 = 8;
    if width == 0 || height == 0 {
        return None;
    }
    let mut first: Option<[u8; 4]> = None;
    for gy in 0..GRID {
        let y = (height - 1) * gy / (GRID - 1);
        for gx in 0..GRID {
            let x = (width - 1) * gx / (GRID - 1);
            let off = y as usize * stride + x as usize * 4;
            let px: [u8; 4] = bytes.get(off..off + 4)?.try_into().ok()?;
            match first {
                None => first = Some(px),
                Some(f) if f != px => return None,
                Some(_) => {}
            }
        }
    }
    let px = first?;
    // Alpha 0 (or all channels 0) = KWin never rendered into the buffer.
    if px[3] == 0 || px == [0, 0, 0, 0] {
        Some(Uniform::Unrendered)
    } else {
        Some(Uniform::Solid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn s(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Connected + settled at t, and one uniform frame right after.
    fn uniform_session(h: &mut Health, t: Instant, kind: Uniform) {
        h.observe(Observation::Connected(t));
        h.observe(Observation::LayoutSettled(t + s(500)));
        h.observe(Observation::Frame {
            sample: Sample::Uniform(kind),
            at: t + s(600),
        });
    }

    #[test]
    fn idle_without_a_session_does_nothing() {
        let mut h = Health::new();
        assert_eq!(h.observe(Observation::Tick(t0())), Action::None);
    }

    #[test]
    fn no_first_frame_recreates_once_then_gives_up() {
        let mut h = Health::new();
        let t = t0();
        h.observe(Observation::Connected(t));
        assert_eq!(h.observe(Observation::Tick(t + s(2_900))), Action::None);
        assert_eq!(h.observe(Observation::Tick(t + s(3_000))), Action::Recreate);
        assert_eq!(h.observe(Observation::Tick(t + s(4_000))), Action::None);
        assert_eq!(h.observe(Observation::Tick(t + s(6_000))), Action::GiveUp);
    }

    #[test]
    fn idle_after_a_real_frame_is_healthy() {
        let mut h = Health::new();
        let t = t0();
        h.observe(Observation::Connected(t));
        h.observe(Observation::LayoutSettled(t + s(500)));
        h.observe(Observation::Frame {
            sample: Sample::Content,
            at: t + s(600),
        });
        for secs in [5, 30, 120, 600] {
            assert_eq!(
                h.observe(Observation::Tick(t + s(secs * 1000))),
                Action::None
            );
        }
    }

    #[test]
    fn a_later_settle_point_voids_the_streak_before_it() {
        let mut h = Health::new();
        let t = t0();
        uniform_session(&mut h, t, Uniform::Solid);
        // Planned plasmashell restart at t+3 s: blank until t+7.5 s is fine.
        h.observe(Observation::LayoutSettled(t + s(7_500)));
        for ms in [600, 4_000, 6_000, 7_000] {
            h.observe(Observation::Frame {
                sample: Sample::Uniform(Uniform::Solid),
                at: t + s(ms),
            });
            assert_eq!(h.observe(Observation::Tick(t + s(ms))), Action::None);
        }
        // Still blank a full streak after the new settle point: heal.
        for ms in [8_000, 10_000, 11_600] {
            h.observe(Observation::Frame {
                sample: Sample::Uniform(Uniform::Solid),
                at: t + s(ms),
            });
        }
        assert_eq!(
            h.observe(Observation::Tick(t + s(12_100))),
            Action::Heal { escalate: false }
        );
    }

    /// Regression: capture samples ~1/s after the connect window, and the
    /// unsampled frames in between used to arrive as "content", resetting
    /// the streak, so a blank desktop was never healed.
    #[test]
    fn unsampled_frames_do_not_break_a_uniform_streak() {
        let mut h = Health::new();
        let t = t0();
        uniform_session(&mut h, t, Uniform::Solid);
        let mut healed = false;
        for ms in (650..6_000).step_by(50) {
            h.observe(Observation::Frame {
                sample: Sample::Skipped,
                at: t + s(ms),
            });
            if ms % 1_000 == 0 {
                h.observe(Observation::Frame {
                    sample: Sample::Uniform(Uniform::Solid),
                    at: t + s(ms),
                });
            }
            if let Action::Heal { .. } = h.observe(Observation::Tick(t + s(ms))) {
                healed = true;
                break;
            }
        }
        assert!(healed, "a blank desktop must heal despite unsampled frames");
    }

    #[test]
    fn a_short_uniform_blip_never_heals() {
        let mut h = Health::new();
        let t = t0();
        uniform_session(&mut h, t, Uniform::Solid);
        assert_eq!(h.observe(Observation::Tick(t + s(3_000))), Action::None);
        h.observe(Observation::Frame {
            sample: Sample::Content,
            at: t + s(3_500),
        });
        assert_eq!(h.observe(Observation::Tick(t + s(10_000))), Action::None);
    }

    #[test]
    fn uniform_before_the_layout_settles_does_not_count() {
        let mut h = Health::new();
        let t = t0();
        h.observe(Observation::Connected(t));
        h.observe(Observation::Frame {
            sample: Sample::Uniform(Uniform::Unrendered),
            at: t + s(100),
        });
        assert_eq!(h.observe(Observation::Tick(t + s(6_000))), Action::None);
    }

    #[test]
    fn uniform_streak_heals_then_escalates_once_then_stops() {
        let mut h = Health::new();
        let t = t0();
        uniform_session(&mut h, t, Uniform::Unrendered);
        assert_eq!(h.observe(Observation::Tick(t + s(4_000))), Action::None);
        assert_eq!(
            h.observe(Observation::Tick(t + s(4_600))),
            Action::Heal { escalate: false }
        );
        assert_eq!(h.observe(Observation::Tick(t + s(12_000))), Action::None);
        assert_eq!(
            h.observe(Observation::Tick(t + s(12_600))),
            Action::Heal { escalate: true }
        );
        assert_eq!(h.observe(Observation::Tick(t + s(50_000))), Action::None);
        assert_eq!(
            h.observe(Observation::Tick(t + s(57_600))),
            Action::StopHealing
        );
        assert_eq!(h.observe(Observation::Tick(t + s(200_000))), Action::None);
    }

    #[test]
    fn real_frame_after_a_heal_resets_the_ladder() {
        let mut h = Health::new();
        let t = t0();
        uniform_session(&mut h, t, Uniform::Unrendered);
        assert_eq!(
            h.observe(Observation::Tick(t + s(4_600))),
            Action::Heal { escalate: false }
        );
        h.observe(Observation::Frame {
            sample: Sample::Content,
            at: t + s(6_000),
        });
        assert_eq!(h.observe(Observation::Tick(t + s(20_000))), Action::None);
    }

    #[test]
    fn escalation_happens_once_per_session() {
        let mut h = Health::new();
        let t = t0();
        uniform_session(&mut h, t, Uniform::Unrendered);
        h.observe(Observation::Tick(t + s(4_600)));
        assert_eq!(
            h.observe(Observation::Tick(t + s(12_600))),
            Action::Heal { escalate: true }
        );
        // A resize re-arms the window; the second uniform streak may heal
        // surgically but must not restart plasmashell again.
        let t2 = t + s(100_000);
        uniform_session(&mut h, t2, Uniform::Unrendered);
        assert_eq!(
            h.observe(Observation::Tick(t2 + s(4_600))),
            Action::Heal { escalate: false }
        );
        assert_eq!(
            h.observe(Observation::Tick(t2 + s(12_600))),
            Action::StopHealing
        );
    }

    #[test]
    fn solid_colour_after_user_input_never_escalates() {
        let mut h = Health::new();
        let t = t0();
        uniform_session(&mut h, t, Uniform::Solid);
        h.observe(Observation::InputInjected);
        assert_eq!(
            h.observe(Observation::Tick(t + s(4_600))),
            Action::Heal { escalate: false }
        );
        assert_eq!(
            h.observe(Observation::Tick(t + s(12_600))),
            Action::StopHealing
        );
    }

    #[test]
    fn uniform_streak_outside_the_connect_window_is_left_alone() {
        let mut h = Health::new();
        let t = t0();
        h.observe(Observation::Connected(t));
        h.observe(Observation::LayoutSettled(t + s(500)));
        h.observe(Observation::Frame {
            sample: Sample::Content,
            at: t + s(600),
        });
        h.observe(Observation::Frame {
            sample: Sample::Uniform(Uniform::Solid),
            at: t + s(30_000),
        });
        assert_eq!(h.observe(Observation::Tick(t + s(40_000))), Action::None);
    }

    #[test]
    fn stream_losses_recreate_until_the_rate_limit() {
        let mut h = Health::new();
        let t = t0();
        for i in 0..3 {
            assert_eq!(
                h.observe(Observation::StreamLost(t + s(i * 1000))),
                Action::Recreate
            );
        }
        assert_eq!(
            h.observe(Observation::StreamLost(t + s(3_000))),
            Action::GiveUp
        );
        // Old losses age out of the window.
        let mut h = Health::new();
        for i in 0..3 {
            h.observe(Observation::StreamLost(t + s(i * 1000)));
        }
        assert_eq!(
            h.observe(Observation::StreamLost(t + s(70_000))),
            Action::Recreate
        );
    }

    #[test]
    fn sampler_classifies_frames() {
        let (w, h, stride) = (64u32, 32u32, 64 * 4 + 16);
        let zero = vec![0u8; stride * h as usize];
        assert_eq!(
            sample_uniform(&zero, w, h, stride),
            Some(Uniform::Unrendered)
        );
        let mut solid = vec![0u8; stride * h as usize];
        for row in solid.chunks_mut(stride) {
            for px in row[..w as usize * 4].chunks_mut(4) {
                px.copy_from_slice(&[10, 20, 30, 255]);
            }
        }
        assert_eq!(sample_uniform(&solid, w, h, stride), Some(Uniform::Solid));
        let mut mixed = solid.clone();
        let off = (h as usize - 1) * stride + (w as usize - 1) * 4;
        mixed[off] = 99;
        assert_eq!(sample_uniform(&mixed, w, h, stride), None);
        // Truncated buffer: not enough evidence → treated as content.
        assert_eq!(sample_uniform(&zero[..100], w, h, stride), None);
    }
}
