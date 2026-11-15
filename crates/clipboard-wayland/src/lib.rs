//! Session clipboard over data-control: `ext_data_control_v1`, falling back
//! to `zwlr_data_control_v1`.
//!
//! data-control is the privileged clipboard protocol clipboard managers use:
//! it sees and sets the seat's selection without a focused surface, which
//! is exactly the position of an RDP server (it never has keyboard focus in
//! the guest). The two protocols are the same shape. KWin serves only the
//! ext one since Plasma 6.4 (wlr support was dropped in April 2025); older
//! KWin, Hyprland and Sway serve wlr. The ext global wins when both exist.
//!
//! One thread owns the Wayland connection (own `Connection`, not shared
//! with the capture pump: a slow paste must never stall frames). The
//! public handle talks to it over a command channel plus a wake pipe that
//! sits in the thread's poll set next to the Wayland fd.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use desk_backend::{BackendError, Clipboard, ClipboardEvent, Result};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self as ext_dev, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self as ext_offer, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self as ext_src, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self as wlr_dev, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self as wlr_offer, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self as wlr_src, ZwlrDataControlSourceV1},
};

/// Marker MIME on our own data source: data-control reports our own
/// selection back as a fresh offer, and this is how we recognise the echo.
/// Kept in sync with rdp-core's `clipformat::OWNER_MARKER_MIME`.
pub const OWNER_MARKER_MIME: &str = "application/x-wayrdp-remote";

enum Cmd {
    Offer(Vec<String>),
    Receive {
        mime: String,
        fd: OwnedFd,
        reply: Sender<bool>,
    },
    Shutdown,
}

/// Write end of the self-pipe that wakes the clipboard thread's poll.
struct Waker(OwnedFd);

impl Waker {
    fn wake(&self) {
        let b = [1u8];
        // Non-blocking: a full pipe already guarantees a pending wake.
        unsafe { libc::write(self.0.as_raw_fd(), b.as_ptr().cast(), 1) };
    }
}

/// data-control clipboard for the current `WAYLAND_DISPLAY`.
pub struct WlClipboard {
    cmd_tx: Option<Sender<Cmd>>,
    waker: Option<Waker>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WlClipboard {
    /// No I/O here (desk-backend contract): the connection opens in `start`.
    pub fn new() -> Self {
        Self {
            cmd_tx: None,
            waker: None,
            thread: None,
        }
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        let (Some(tx), Some(w)) = (self.cmd_tx.as_ref(), self.waker.as_ref()) else {
            return Err(BackendError::Unavailable("clipboard not started".into()));
        };
        tx.send(cmd)
            .map_err(|_| BackendError::Unavailable("clipboard thread gone".into()))?;
        w.wake();
        Ok(())
    }
}

impl Default for WlClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WlClipboard {
    fn drop(&mut self) {
        let _ = self.send(Cmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Clipboard for WlClipboard {
    fn start(&mut self, tx: Sender<ClipboardEvent>) -> Result<()> {
        let (rfd, wfd) = pipe(libc::O_NONBLOCK)?;
        let (cmd_tx, cmd_rx) = unbounded::<Cmd>();
        let (ready_tx, ready_rx) = bounded::<std::result::Result<(), String>>(1);
        let thread = std::thread::Builder::new()
            .name("wayrdp-clipboard".into())
            .spawn(move || run(tx, cmd_rx, rfd, ready_tx))
            .map_err(|e| BackendError::Other(e.into()))?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => {}
            Ok(Err(why)) => {
                let _ = thread.join();
                return Err(BackendError::Unavailable(why));
            }
            Err(_) => {
                return Err(BackendError::Unavailable(
                    "clipboard thread did not come up in 3s".into(),
                ))
            }
        }
        self.cmd_tx = Some(cmd_tx);
        self.waker = Some(Waker(wfd));
        self.thread = Some(thread);
        Ok(())
    }

    fn offer_remote(&self, mimes: Vec<String>) -> Result<()> {
        self.send(Cmd::Offer(mimes))
    }

    fn read_local(&self, mime: &str, timeout: Duration, max_bytes: usize) -> Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let (rfd, wfd) = pipe(0)?;
        let (reply_tx, reply_rx) = bounded(1);
        self.send(Cmd::Receive {
            mime: mime.to_string(),
            fd: wfd,
            reply: reply_tx,
        })?;
        match reply_rx.recv_timeout(timeout) {
            Ok(true) => {}
            Ok(false) => {
                return Err(BackendError::Unavailable(format!(
                    "no local selection offers {mime}"
                )))
            }
            Err(_) => return Err(BackendError::Unavailable("clipboard thread busy".into())),
        }
        read_until_eof(rfd, deadline, max_bytes)
            .map_err(|e| BackendError::Other(anyhow::anyhow!("reading {mime}: {e}")))
    }
}

/// `pipe2(O_CLOEXEC | extra)` → (read, write).
fn pipe(extra: libc::c_int) -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | extra) } != 0 {
        return Err(BackendError::Other(std::io::Error::last_os_error().into()));
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn poll_in(fds: &mut [libc::pollfd], timeout_ms: i32) -> i32 {
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) }
}

/// Drain a pipe until EOF, bounded in time and size. The source application
/// writes at its own pace (and may never close if it hangs).
fn read_until_eof(fd: OwnedFd, deadline: Instant, max: usize) -> std::io::Result<Vec<u8>> {
    let mut file = File::from(fd);
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "source did not finish writing",
            ));
        }
        let mut pfd = [libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if poll_in(&mut pfd, left.as_millis().min(i32::MAX as u128) as i32) <= 0 {
            continue;
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        if out.len() + n > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!("selection exceeds {max} bytes"),
            ));
        }
        out.extend_from_slice(&buf[..n]);
    }
}

// ---------------------------------------------------------------------------
// Protocol-neutral wrappers. ext-data-control and wlr-data-control are the
// same protocol under two names, so each object is one of two typed
// proxies; a session only ever holds objects of the protocol it bound.

enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl Manager {
    fn protocol(&self) -> &'static str {
        match self {
            Manager::Ext(_) => "ext_data_control_v1",
            Manager::Wlr(_) => "zwlr_data_control_v1",
        }
    }

    fn get_device(&self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) -> Device {
        match self {
            Manager::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
            Manager::Wlr(m) => Device::Wlr(m.get_data_device(seat, qh, ())),
        }
    }

    fn create_source(&self, qh: &QueueHandle<State>) -> Source {
        match self {
            Manager::Ext(m) => Source::Ext(m.create_data_source(qh, ())),
            Manager::Wlr(m) => Source::Wlr(m.create_data_source(qh, ())),
        }
    }
}

impl Device {
    fn set_selection(&self, source: &Source) {
        match (self, source) {
            (Device::Ext(d), Source::Ext(s)) => d.set_selection(Some(s)),
            (Device::Wlr(d), Source::Wlr(s)) => d.set_selection(Some(s)),
            // Both come from the one bound manager; a mix cannot happen.
            _ => {}
        }
    }

    fn destroy(&self) {
        match self {
            Device::Ext(d) => d.destroy(),
            Device::Wlr(d) => d.destroy(),
        }
    }
}

impl Offer {
    fn id(&self) -> ObjectId {
        match self {
            Offer::Ext(o) => o.id(),
            Offer::Wlr(o) => o.id(),
        }
    }

    fn receive(&self, mime: String, fd: std::os::fd::BorrowedFd<'_>) {
        match self {
            Offer::Ext(o) => o.receive(mime, fd),
            Offer::Wlr(o) => o.receive(mime, fd),
        }
    }

    fn destroy(&self) {
        match self {
            Offer::Ext(o) => o.destroy(),
            Offer::Wlr(o) => o.destroy(),
        }
    }
}

impl Source {
    fn id(&self) -> ObjectId {
        match self {
            Source::Ext(s) => s.id(),
            Source::Wlr(s) => s.id(),
        }
    }

    fn offer(&self, mime: String) {
        match self {
            Source::Ext(s) => s.offer(mime),
            Source::Wlr(s) => s.offer(mime),
        }
    }

    fn destroy(&self) {
        match self {
            Source::Ext(s) => s.destroy(),
            Source::Wlr(s) => s.destroy(),
        }
    }
}

struct State {
    seat: Option<wl_seat::WlSeat>,
    /// `(name, version)` of each data-control global the registry announced.
    ext_global: Option<(u32, u32)>,
    wlr_global: Option<(u32, u32)>,
    device: Option<Device>,
    /// MIME lists of offers announced but not yet made the selection.
    pending_mimes: HashMap<ObjectId, Vec<String>>,
    current_offer: Option<Offer>,
    our_source: Option<Source>,
    events: Sender<ClipboardEvent>,
    finished: bool,
}

fn run(
    events: Sender<ClipboardEvent>,
    cmd_rx: Receiver<Cmd>,
    wake_rfd: OwnedFd,
    ready: Sender<std::result::Result<(), String>>,
) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(format!("Wayland connect failed: {e}")));
            return;
        }
    };
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let mut state = State {
        seat: None,
        ext_global: None,
        wlr_global: None,
        device: None,
        pending_mimes: HashMap::new(),
        current_offer: None,
        our_source: None,
        events,
        finished: false,
    };
    if queue.roundtrip(&mut state).is_err() {
        let _ = ready.send(Err("registry roundtrip failed".into()));
        return;
    }
    let manager = match (state.ext_global, state.wlr_global) {
        (Some((name, version)), _) => Manager::Ext(registry.bind(name, version.min(1), &qh, ())),
        (None, Some((name, version))) => Manager::Wlr(registry.bind(name, version.min(2), &qh, ())),
        (None, None) => {
            let _ = ready.send(Err(
                "compositor offers neither ext_data_control_manager_v1 nor \
                 zwlr_data_control_manager_v1"
                    .into(),
            ));
            return;
        }
    };
    let Some(seat) = state.seat.clone() else {
        let _ = ready.send(Err("compositor offers no wl_seat".into()));
        return;
    };
    state.device = Some(manager.get_device(&seat, &qh));
    // Second roundtrip delivers the initial selection (if any) so the client
    // learns about a pre-existing guest clipboard right away.
    let _ = queue.roundtrip(&mut state);
    let _ = ready.send(Ok(()));
    eprintln!(
        "[clipboard] data-control clipboard up ({})",
        manager.protocol()
    );

    let mut drain = [0u8; 64];
    'main: loop {
        for cmd in cmd_rx.try_iter() {
            match cmd {
                Cmd::Shutdown => break 'main,
                Cmd::Offer(mimes) => {
                    let source = manager.create_source(&qh);
                    for m in &mimes {
                        source.offer(m.clone());
                    }
                    source.offer(OWNER_MARKER_MIME.to_string());
                    if let Some(dev) = state.device.as_ref() {
                        dev.set_selection(&source);
                    }
                    if let Some(old) = state.our_source.replace(source) {
                        old.destroy();
                    }
                }
                Cmd::Receive { mime, fd, reply } => {
                    let ok = match state.current_offer.as_ref() {
                        Some(offer) => {
                            offer.receive(mime, fd.as_fd());
                            true
                        }
                        None => false,
                    };
                    // The fd travels with the request on flush; our copy
                    // must close right after, or the reader never sees EOF.
                    let _ = conn.flush();
                    drop(fd);
                    let _ = reply.send(ok);
                }
            }
        }
        if state.finished {
            eprintln!("[clipboard] data-control device finished; clipboard thread exiting");
            break;
        }

        let _ = queue.flush();
        if let Some(guard) = queue.prepare_read() {
            let mut fds = [
                libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake_rfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // The timeout is only a backstop; the wake pipe delivers commands.
            if poll_in(&mut fds, 1000) > 0 && fds[0].revents & libc::POLLIN != 0 {
                if let Err(e) = guard.read() {
                    eprintln!("[clipboard] wayland read error: {e:?}");
                    break;
                }
            } else {
                drop(guard);
            }
            if fds[1].revents & libc::POLLIN != 0 {
                while unsafe {
                    libc::read(wake_rfd.as_raw_fd(), drain.as_mut_ptr().cast(), drain.len())
                } > 0
                {}
            }
        }
        if let Err(e) = queue.dispatch_pending(&mut state) {
            eprintln!("[clipboard] wayland dispatch error: {e}");
            break;
        }
    }

    if let Some(src) = state.our_source.take() {
        src.destroy();
    }
    if let Some(offer) = state.current_offer.take() {
        offer.destroy();
    }
    if let Some(dev) = state.device.take() {
        dev.destroy();
    }
    let _ = conn.flush();
}

// ---------------------------------------------------------------------------
// Protocol-neutral event handling, shared by both protocols' dispatch.

fn on_selection(state: &mut State, offer: Option<Offer>) {
    if let Some(old) = state.current_offer.take() {
        old.destroy();
    }
    let mimes = match offer {
        Some(offer) => {
            let mimes = state.pending_mimes.remove(&offer.id()).unwrap_or_default();
            state.current_offer = Some(offer);
            mimes
        }
        None => Vec::new(),
    };
    // Our own selection echoing back: the remote side already knows what it
    // offered.
    if mimes.iter().any(|m| m == OWNER_MARKER_MIME) {
        return;
    }
    let _ = state.events.send(ClipboardEvent::LocalSelection { mimes });
}

fn on_offer_mime(state: &mut State, offer: &ObjectId, mime: String) {
    if let Some(list) = state.pending_mimes.get_mut(offer) {
        list.push(mime);
    }
}

fn on_send(state: &mut State, mime: String, fd: OwnedFd) {
    if mime == OWNER_MARKER_MIME {
        return; // nothing to send; dropping fd closes it
    }
    let mut sink = File::from(fd);
    // The fd may arrive non-blocking; the consumer writes from a worker
    // thread and wants plain blocking writes.
    unsafe {
        let flags = libc::fcntl(sink.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(sink.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
    let _ = sink.flush();
    let _ = state.events.send(ClipboardEvent::SendRequest {
        mime,
        sink: Box::new(sink),
    });
}

fn on_cancelled(state: &mut State, source: Source) {
    if state.our_source.as_ref().map(Source::id) == Some(source.id()) {
        state.our_source = None;
    }
    source.destroy();
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
                "ext_data_control_manager_v1" => state.ext_global = Some((name, version)),
                "zwlr_data_control_manager_v1" => state.wlr_global = Some((name, version)),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Dispatch for one data-control protocol: `$wrap` is the wrapper variant,
/// the rest are that protocol's proxy types and event modules.
macro_rules! data_control_dispatch {
    ($wrap:ident, $mgr:ty, $dev:ty, $dev_m:ident, $offer:ty, $offer_m:ident, $src:ty, $src_m:ident) => {
        impl Dispatch<$mgr, ()> for State {
            fn event(
                _: &mut Self,
                _: &$mgr,
                _: <$mgr as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }

        impl Dispatch<$dev, ()> for State {
            fn event(
                state: &mut Self,
                _: &$dev,
                event: $dev_m::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $dev_m::Event::DataOffer { id } => {
                        state.pending_mimes.insert(id.id(), Vec::new());
                    }
                    $dev_m::Event::Selection { id } => on_selection(state, id.map(Offer::$wrap)),
                    $dev_m::Event::PrimarySelection { id } => {
                        // Primary selection (middle-click) is not bridged.
                        if let Some(offer) = id {
                            state.pending_mimes.remove(&offer.id());
                            offer.destroy();
                        }
                    }
                    $dev_m::Event::Finished => state.finished = true,
                    _ => {}
                }
            }

            event_created_child!(State, $dev, [
                $dev_m::EVT_DATA_OFFER_OPCODE => ($offer, ()),
            ]);
        }

        impl Dispatch<$offer, ()> for State {
            fn event(
                state: &mut Self,
                offer: &$offer,
                event: $offer_m::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let $offer_m::Event::Offer { mime_type } = event {
                    on_offer_mime(state, &offer.id(), mime_type);
                }
            }
        }

        impl Dispatch<$src, ()> for State {
            fn event(
                state: &mut Self,
                source: &$src,
                event: $src_m::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $src_m::Event::Send { mime_type, fd } => on_send(state, mime_type, fd),
                    $src_m::Event::Cancelled => on_cancelled(state, Source::$wrap(source.clone())),
                    _ => {}
                }
            }
        }
    };
}

data_control_dispatch!(
    Ext,
    ExtDataControlManagerV1,
    ExtDataControlDeviceV1,
    ext_dev,
    ExtDataControlOfferV1,
    ext_offer,
    ExtDataControlSourceV1,
    ext_src
);
data_control_dispatch!(
    Wlr,
    ZwlrDataControlManagerV1,
    ZwlrDataControlDeviceV1,
    wlr_dev,
    ZwlrDataControlOfferV1,
    wlr_offer,
    ZwlrDataControlSourceV1,
    wlr_src
);
