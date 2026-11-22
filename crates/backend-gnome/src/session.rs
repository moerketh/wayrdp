//! The "gnome-session" thread: one current-thread tokio runtime that owns
//! the Mutter session for the whole connection.
//!
//! Order:
//! 1. open the RemoteDesktop + ScreenCast pair and the virtual monitor, and
//!    connect PipeWire with a fixed-size offer: that offer is what sizes the
//!    virtual monitor, so it only appears in DisplayConfig afterwards;
//! 2. engage the layout guard (only our monitor enabled) once it appears,
//!    then connect EIS;
//! 3. loop: resize (renegotiate the stream), MonitorsChanged (re-check the
//!    guard), Mutter closing the session (recreate), health, capture events;
//! 4. teardown: input off, capture off, RemoteDesktop.Stop. Mutter restores
//!    the physical monitor itself (S1); we verify and repair if it did not.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capture_pipewire::capture::{self, Command, CommandTx};
use capture_pipewire::health::{Action, Health, Observation, Sample};
use desk_backend::resize::{sanitize, Coalescer};
use desk_backend::Size;
use futures_util::StreamExt as _;
use tokio::sync::{mpsc::UnboundedReceiver, Notify};

use crate::input::Input;
use crate::layout;
use crate::mutter::{self, Owner};

/// The stream renegotiates and Mutter relayouts; uniform frames before this
/// are the expected relayout, not a fault.
const CREATE_SETTLE: Duration = Duration::from_millis(2_500);
const TICK: Duration = Duration::from_millis(500);
/// How long the virtual monitor may take to show up in DisplayConfig after
/// the PipeWire offer.
const MONITOR_WAIT: Duration = Duration::from_secs(5);
/// Wait for a previous session's teardown (vmconnect reconnects arrive
/// while the old peer is still tearing down — the normal case).
const LEASE_WAIT: Duration = Duration::from_secs(20);
/// After teardown, how long Mutter gets to bring the console back.
const RESTORE_WAIT: Duration = Duration::from_secs(3);

static SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Process-wide: the layout guard is global, so only one session at a time.
struct Lease;

impl Drop for Lease {
    fn drop(&mut self) {
        SESSION_ACTIVE.store(false, Ordering::SeqCst);
        eprintln!("[backend-gnome] teardown: session lease released");
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
    pub scale: f64,
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
            eprintln!("[backend-gnome] session: no tokio runtime: {e}");
            let _ = s.capture.send(Command::Shutdown);
            return;
        }
    };
    rt.block_on(session(s));
}

struct Live {
    owner: Owner,
    /// Our virtual monitor's connector, once it shows up.
    ours: Option<String>,
    capture: CommandTx,
    input: Arc<Input>,
    health: Health,
    coalescer: Coalescer,
    scale: f64,
    changes: Option<zbus::MessageStream>,
}

/// Open a Mutter session at `size`, retrying once.
async fn open(size: Size) -> Option<Owner> {
    for attempt in 0..2 {
        match mutter::open().await {
            Ok(o) => return Some(o),
            Err(e) => {
                eprintln!(
                    "[backend-gnome] session: virtual monitor {}x{} failed (attempt {}): {e:#}",
                    size.width,
                    size.height,
                    attempt + 1
                );
                if format!("{e:#}").contains("inhibited") {
                    // gnome-shell inhibits remote access whenever its session
                    // mode forbids screencasts: the lock screen, mostly.
                    eprintln!(
                        "[backend-gnome] session: GNOME refuses remote desktop while the screen \
                         is locked; disable the lock (org.gnome.desktop.screensaver lock-enabled \
                         false) or unlock the console"
                    );
                    return None;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    None
}

impl Live {
    /// Point capture and input at the current owner and wait for the
    /// monitor, then guard it.
    async fn attach(&mut self, size: Size) {
        let now = Instant::now();
        let node = self.owner.node;
        let _ = self.capture.send(Command::Connect {
            node,
            size: Some(size),
        });
        self.health.observe(Observation::Connected(now));
        self.health
            .observe(Observation::LayoutSettled(now + CREATE_SETTLE));
        self.ours = self.wait_for_monitor(size).await;
        match &self.ours {
            Some(c) => eprintln!("[backend-gnome] session: virtual monitor {c} on node {node}"),
            None => eprintln!(
                "[backend-gnome] session: virtual monitor not in DisplayConfig after {MONITOR_WAIT:?}; \
                 streaming without the layout guard"
            ),
        }
        self.guard().await;
        self.input
            .retarget(self.owner.eis_target(), self.owner.mapping_id().await);
        self.input.set_size(size.width, size.height);
        self.input.connect();
        self.changes = mutter::monitors_changed(&self.owner.conn).await.ok();
    }

    async fn wait_for_monitor(&self, size: Size) -> Option<String> {
        let deadline = Instant::now() + MONITOR_WAIT;
        loop {
            if let Ok(st) = mutter::state(&self.owner.conn).await {
                if let Some(m) =
                    layout::find_ours(&self.owner.before, &st, (size.width, size.height))
                {
                    if m.current_mode.is_some() {
                        return Some(m.connector.clone());
                    }
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Make ours the only enabled monitor, unless it already is.
    async fn guard(&self) {
        let Some(ours) = &self.ours else { return };
        let st = match mutter::state(&self.owner.conn).await {
            Ok(st) => st,
            Err(e) => {
                eprintln!("[backend-gnome] layout: GetCurrentState: {e:#}");
                return;
            }
        };
        if layout::guarded(&st, ours) {
            return;
        }
        let Some(cfg) = layout::guard_config(&st, ours, self.scale) else {
            return;
        };
        match mutter::apply(&self.owner.conn, st.serial, &cfg).await {
            Ok(()) => eprintln!("[backend-gnome] layout: guard engaged ({ours} only)"),
            Err(e) => eprintln!("[backend-gnome] layout: guard apply failed: {e:#}"),
        }
    }

    /// Replace the Mutter session (new monitor, new node, new EIS).
    async fn recreate(&mut self) -> bool {
        let size = self.coalescer.current();
        self.owner.stop().await;
        let Some(owner) = open(size).await else {
            return false;
        };
        self.owner = owner;
        self.attach(size).await;
        true
    }

    async fn resize(&mut self, size: Size) {
        eprintln!(
            "[backend-gnome] session: resize to {}x{}",
            size.width, size.height
        );
        let _ = self.capture.send(Command::Resize(size));
        self.coalescer.applied(size);
        let now = Instant::now();
        self.health.observe(Observation::Connected(now));
        self.health
            .observe(Observation::LayoutSettled(now + CREATE_SETTLE));
        self.input.set_size(size.width, size.height);
    }

    /// Run one health action. `false` = end the session.
    async fn act(&mut self, action: Action) -> bool {
        match action {
            Action::None => true,
            Action::Heal { escalate } => {
                // No shell restart on GNOME: gnome-shell is the compositor.
                eprintln!("[backend-gnome] health: layout heal (escalate={escalate})");
                self.guard().await;
                true
            }
            Action::Recreate => {
                eprintln!("[backend-gnome] health: recreating the virtual monitor");
                self.recreate().await
            }
            Action::StopHealing => {
                eprintln!(
                    "[backend-gnome] health: desktop still blank after healing; giving up on heals \
                     (streaming continues)"
                );
                true
            }
            Action::GiveUp => {
                eprintln!("[backend-gnome] health: no usable stream; ending the session");
                false
            }
        }
    }
}

async fn next_change(changes: &mut Option<zbus::MessageStream>) -> bool {
    match changes {
        Some(s) => s.next().await.is_some(),
        None => std::future::pending().await,
    }
}

async fn session(s: Session) {
    let Session {
        initial,
        scale,
        control,
        capture,
        mut capture_events,
        input,
    } = s;
    let Some(_lease) = claim_lease().await else {
        eprintln!(
            "[backend-gnome] session: previous session did not release in {}s; refusing",
            LEASE_WAIT.as_secs()
        );
        let _ = capture.send(Command::Shutdown);
        return;
    };
    if control.shutdown.load(Ordering::SeqCst) {
        eprintln!("[backend-gnome] session: client left before the monitor was created");
        let _ = capture.send(Command::Shutdown);
        return;
    }
    let (initial, clamped) = sanitize(initial);
    if clamped {
        eprintln!(
            "[backend-gnome] session: client size clamped to {}x{}",
            initial.width, initial.height
        );
    }
    let started = Instant::now();
    let Some(owner) = open(initial).await else {
        let _ = capture.send(Command::Shutdown);
        return;
    };
    let mut live = Live {
        owner,
        ours: None,
        capture: capture.clone(),
        input: input.clone(),
        health: Health::new(),
        coalescer: Coalescer::new(initial),
        scale,
        changes: None,
    };
    live.attach(initial).await;
    eprintln!(
        "[backend-gnome] session: live at {}x{} on node {} ({} ms)",
        initial.width,
        initial.height,
        live.owner.node,
        started.elapsed().as_millis()
    );

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
                                "[backend-gnome] capture: first frame {} ms after create",
                                at.saturating_duration_since(started).as_millis()
                            );
                        }
                        if sample != Sample::Skipped && Some(sample) != last_sample {
                            last_sample = Some(sample);
                            eprintln!("[backend-gnome] health: desktop content {sample:?}");
                        }
                        live.health.observe(Observation::Frame { sample, at })
                    }
                    Some(capture::Event::StreamLost(at)) => {
                        live.health.observe(Observation::StreamLost(at))
                    }
                    None => Action::GiveUp,
                };
                if !live.act(action).await {
                    break;
                }
            }
            closed = live.owner.closed.next() => {
                if closed.is_none() {
                    continue;
                }
                eprintln!("[backend-gnome] session: Mutter closed the remote-desktop session");
                let action = live.health.observe(Observation::StreamLost(Instant::now()));
                let action = if action == Action::GiveUp { action } else { Action::Recreate };
                if !live.act(action).await {
                    break;
                }
            }
            changed = next_change(&mut live.changes) => {
                if changed {
                    live.guard().await;
                } else {
                    live.changes = None;
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
                    live.resize(size).await;
                }
            }
        }
    }

    eprintln!("[backend-gnome] teardown: begin");
    input.shutdown();
    let _ = capture.send(Command::Disconnect);
    live.owner.stop().await;
    restore(&live.owner.conn).await;
    let _ = capture.send(Command::Shutdown);
    eprintln!("[backend-gnome] teardown: done");
}

/// Wait for Mutter to bring a physical monitor back; apply a restore
/// layout if it did not.
pub async fn restore(conn: &zbus::Connection) {
    let deadline = Instant::now() + RESTORE_WAIT;
    loop {
        match mutter::state(conn).await {
            Ok(st) if layout::physical_enabled(&st) => return,
            Ok(st) if Instant::now() >= deadline => {
                let Some(cfg) = layout::restore_config(&st) else {
                    eprintln!("[backend-gnome] layout: no physical monitor to restore");
                    return;
                };
                match mutter::apply(conn, st.serial, &cfg).await {
                    Ok(()) => eprintln!("[backend-gnome] layout: physical monitors restored"),
                    Err(e) => eprintln!("[backend-gnome] layout: restore failed: {e:#}"),
                }
                return;
            }
            Err(e) if Instant::now() >= deadline => {
                eprintln!("[backend-gnome] layout: restore check failed: {e:#}");
                return;
            }
            _ => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}
