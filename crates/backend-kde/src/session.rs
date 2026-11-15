//! The "kde-session" thread: one current-thread tokio runtime driving the
//! extras session for the whole connection (extras `tokio::spawn`s
//! containment-reattach tasks that need a live runtime for ~4 s after each
//! output create).
//!
//! Order matters and is fixed here:
//! 1. create the virtual output, connect PipeWire, kick EIS — frames can
//!    start flowing before the guard settles (first connect < 2 s);
//! 2. engage the layout guard (physical outputs off) — only after the
//!    output exists, as extras requires;
//! 3. loop: resize coalescing, health ticks, capture events;
//! 4. teardown: input off, capture off, guard.finish() (physical outputs
//!    back) BEFORE close_stream() — the reverse leaves a dead shell on the
//!    next connect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use desk_backend::Size;
use hyperv_rdp_extras::session::{
    heal_output_layout, ModeChangeOutcome, OutputLayoutGuard, VirtualOutputConfig,
    VirtualOutputManager,
};
use tokio::sync::{mpsc::UnboundedReceiver, Notify};

use crate::capture::{self, Command, CommandTx};
use crate::health::{Action, Health, Observation, Sample};
use crate::input::Input;
use crate::recovery::KSCREEN_NAME;
use crate::resize::{sanitize, Coalescer};

/// Name passed to zkde `stream_virtual_output` (kscreen: `Virtual-wayrdp`).
pub const OUTPUT_NAME: &str = "wayrdp";
/// extras re-adopts plasmashell containments 0.75 s and 3.25 s after each
/// create and normalizes the origin ~0.5 s later; uniform frames before
/// that are the expected relayout, not a fault.
const CREATE_SETTLE: Duration = Duration::from_millis(4_500);
const TICK: Duration = Duration::from_millis(500);
/// Wait for a previous session's teardown (vmconnect reconnects arrive
/// while the old peer is still tearing down — the normal case).
const LEASE_WAIT: Duration = Duration::from_secs(20);

static SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Process-wide: KWin has one `Virtual-wayrdp` identity and the layout
/// guard is global, so only one KDE session may run at a time.
struct Lease;

impl Drop for Lease {
    fn drop(&mut self) {
        SESSION_ACTIVE.store(false, Ordering::SeqCst);
        eprintln!("[backend-kde] teardown: session lease released");
    }
}

async fn claim_lease() -> Option<Lease> {
    let start = Instant::now();
    loop {
        if SESSION_ACTIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Some(Lease);
        }
        if start.elapsed() > LEASE_WAIT {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Shared between the capture session (writer) and the session task.
#[derive(Default)]
pub struct Control {
    pub resize: Mutex<Option<Size>>,
    pub shutdown: AtomicBool,
    pub notify: Notify,
}

impl Control {
    pub fn request_resize(&self, size: Size) {
        *self.resize.lock().unwrap_or_else(|p| p.into_inner()) = Some(size);
        self.notify.notify_one();
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

pub struct Session {
    pub initial: Size,
    pub control: Arc<Control>,
    pub capture: CommandTx,
    pub capture_events: UnboundedReceiver<capture::Event>,
    pub input: Arc<Input>,
}

/// Thread body. Returns when the session is over and torn down.
pub fn run(s: Session) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[backend-kde] session: no tokio runtime: {e}");
            let _ = s.capture.send(Command::Shutdown);
            return;
        }
    };
    rt.block_on(session(s));
}

/// Create (or swap to) the output at `size`, retrying once.
async fn create(mgr: &VirtualOutputManager, size: Size) -> Option<u32> {
    for attempt in 0..2 {
        match mgr
            .recreate_stream(size.width as u16, size.height as u16)
            .await
        {
            Ok(node) => return Some(node),
            Err(e) => {
                eprintln!(
                    "[backend-kde] session: virtual output {}x{} failed (attempt {}): {e:#}",
                    size.width,
                    size.height,
                    attempt + 1
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    None
}

struct Live {
    mgr: VirtualOutputManager,
    capture: CommandTx,
    health: Health,
    coalescer: Coalescer,
    /// `None` until the first in-place attempt says whether this KWin can.
    in_place: Option<bool>,
}

impl Live {
    fn connected(&mut self, node: u32) {
        let now = Instant::now();
        let _ = self.capture.send(Command::Connect(node));
        self.health.observe(Observation::Connected(now));
        self.health
            .observe(Observation::LayoutSettled(now + CREATE_SETTLE));
    }

    async fn recreate(&mut self, size: Size) -> bool {
        match create(&self.mgr, size).await {
            Some(node) => {
                self.connected(node);
                self.coalescer.applied(size);
                true
            }
            None => false,
        }
    }

    /// Apply a coalesced resize: in place when KWin can (≥ 6.7: same output
    /// and PipeWire node, no containment churn), else recreate.
    async fn resize(&mut self, size: Size) -> bool {
        eprintln!(
            "[backend-kde] session: resize to {}x{}",
            size.width, size.height
        );
        if self.in_place != Some(false) {
            let outcome = self
                .mgr
                .try_change_mode_in_place(size.width as u16, size.height as u16)
                .await;
            match outcome {
                ModeChangeOutcome::Applied => {
                    self.in_place = Some(true);
                    self.coalescer.applied(size);
                    // Same node: the stream renegotiates its format.
                    let now = Instant::now();
                    self.health.observe(Observation::Connected(now));
                    self.health.observe(Observation::LayoutSettled(now));
                    return true;
                }
                // Stable per KWin version: don't ask again.
                ModeChangeOutcome::NotAdvertised | ModeChangeOutcome::Unsupported(_) => {
                    eprintln!(
                        "[backend-kde] session: in-place mode change unavailable ({outcome:?}); \
                         resizes recreate the output"
                    );
                    self.in_place = Some(false);
                }
                other => {
                    eprintln!("[backend-kde] session: in-place mode change {other:?}; recreating");
                }
            }
        }
        self.recreate(size).await
    }

    /// Run one health action. `false` = end the session.
    async fn act(&mut self, action: Action) -> bool {
        match action {
            Action::None => true,
            Action::Heal { escalate } => {
                eprintln!("[backend-kde] health: layout heal (escalate={escalate})");
                let ok = heal_output_layout(KSCREEN_NAME, escalate).await;
                eprintln!("[backend-kde] health: heal done, origin verified: {ok}");
                true
            }
            Action::Recreate => {
                eprintln!("[backend-kde] health: recreating the virtual output");
                let size = self.coalescer.current();
                self.recreate(size).await
            }
            Action::StopHealing => {
                eprintln!(
                    "[backend-kde] health: desktop still blank after healing; giving up on heals \
                     (streaming continues)"
                );
                true
            }
            Action::GiveUp => {
                eprintln!("[backend-kde] health: no usable stream; ending the session");
                false
            }
        }
    }
}

async fn session(s: Session) {
    let Session {
        initial,
        control,
        capture,
        mut capture_events,
        input,
    } = s;
    let Some(_lease) = claim_lease().await else {
        eprintln!(
            "[backend-kde] session: previous session did not release in {}s; refusing",
            LEASE_WAIT.as_secs()
        );
        let _ = capture.send(Command::Shutdown);
        return;
    };
    // The peer may have gone while we waited for the previous session.
    if control.shutdown.load(Ordering::SeqCst) {
        eprintln!("[backend-kde] session: client left before the output was created");
        let _ = capture.send(Command::Shutdown);
        return;
    }

    let cfg = VirtualOutputConfig::new(OUTPUT_NAME);
    let (initial, clamped) = sanitize(initial);
    if clamped {
        eprintln!(
            "[backend-kde] session: client size clamped to {}x{}",
            initial.width, initial.height
        );
    }
    let mut live = Live {
        mgr: VirtualOutputManager::with_config(cfg.clone()),
        capture: capture.clone(),
        health: Health::new(),
        coalescer: Coalescer::new(initial),
        in_place: None,
    };
    let started = Instant::now();
    let Some(node) = create(&live.mgr, initial).await else {
        let _ = capture.send(Command::Shutdown);
        return;
    };
    live.connected(node);
    input.connect();
    let mut guard = OutputLayoutGuard::engage_with(cfg).await;
    eprintln!(
        "[backend-kde] session: live at {}x{} on node {node} ({} ms)",
        initial.width,
        initial.height,
        started.elapsed().as_millis()
    );
    let settled = Instant::now().max(started + CREATE_SETTLE);
    live.health.observe(Observation::LayoutSettled(settled));

    let mut tick = tokio::time::interval(TICK);
    let mut input_noted = false;
    let mut first_frame = true;
    let mut last_sample: Option<Sample> = None;
    loop {
        let due = live.coalescer.deadline();
        tokio::select! {
            _ = control.notify.notified() => {
                if control.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let req = control.resize.lock().unwrap_or_else(|p| p.into_inner()).take();
                if let Some(size) = req {
                    live.coalescer.request(sanitize(size).0, Instant::now());
                }
            }
            ev = capture_events.recv() => {
                let action = match ev {
                    Some(capture::Event::Frame { sample, at }) => {
                        if first_frame {
                            first_frame = false;
                            eprintln!(
                                "[backend-kde] capture: first frame {} ms after create",
                                at.saturating_duration_since(started).as_millis()
                            );
                        }
                        // Transitions only (harness A7 greps "desktop content").
                        if sample != Sample::Skipped && Some(sample) != last_sample {
                            last_sample = Some(sample);
                            eprintln!("[backend-kde] health: desktop content {sample:?}");
                        }
                        live.health.observe(Observation::Frame { sample, at })
                    }
                    Some(capture::Event::StreamLost(at)) => {
                        live.health.observe(Observation::StreamLost(at))
                    }
                    None => Action::GiveUp, // capture thread gone
                };
                if !live.act(action).await {
                    break;
                }
            }
            _ = tick.tick() => {
                if !input_noted && input.input_seen.load(Ordering::Relaxed) {
                    input_noted = true;
                    live.health.observe(Observation::InputInjected);
                }
                let action = live.health.observe(Observation::Tick(Instant::now()));
                if !live.act(action).await {
                    break;
                }
            }
            _ = tokio::time::sleep_until(due.unwrap_or_else(Instant::now).into()), if due.is_some() => {
                if let Some(size) = live.coalescer.due(Instant::now()) {
                    if !live.resize(size).await {
                        break;
                    }
                }
            }
        }
    }

    // Teardown, in this exact order.
    eprintln!("[backend-kde] teardown: begin");
    input.shutdown();
    let _ = capture.send(Command::Disconnect);
    guard.finish().await;
    live.mgr.close_stream().await;
    // close_stream only queues the command; give the extras Wayland thread
    // time to flush it before the manager (its sender) drops.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = capture.send(Command::Shutdown);
    eprintln!("[backend-kde] teardown: done");
}
