//! Input injection through KWin's EIS endpoint (the "kde-input" thread).
//!
//! `org.kde.KWin.EIS.RemoteDesktop.connectToEIS` hands a non-portal client
//! a libei socket with no consent dialog (KWin ≥ 6.1). KWin ties that EIS
//! context to the caller's D-Bus name and drops it when the name vanishes,
//! so the D-Bus connection lives as long as the input session.
//!
//! KWin recreates its absolute-pointer device on every screen change and
//! its keyboard device on every keymap change, so devices are tracked as
//! they come and go: each event goes to a resumed device that has the
//! needed interface at that moment.
//!
//! The thread owns one poll loop over the EIS socket and a wake eventfd;
//! `inject` only pushes onto an unbounded channel and bumps the eventfd, so
//! it never blocks (`inject_deadline` is trivially met) and a multi-second
//! kscreen heal on the session thread can't stall typing.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use desk_backend::InputEvent;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent, EiEventConverter};
use xkbcommon::xkb;

use crate::input_map::{self, Locks};

const EIS_PATH: &str = "/org/kde/KWin/EIS/RemoteDesktop";
const EIS_IFACE: &str = "org.kde.KWin.EIS.RemoteDesktop";
/// connectToEIS capability bits (KWin's portal values): keyboard | pointer.
const EIS_CAPS: i32 = 1 | 2;
/// Reconnect backoff after the EIS socket hangs up (KWin restart).
const BACKOFF: [u64; 5] = [1, 2, 5, 5, 5];

enum Msg {
    Input(InputEvent),
    /// Connect (or reconnect) to EIS; sent once the virtual output exists so
    /// the absolute region covers it.
    Connect,
    /// Current virtual output size, used to pick the right absolute region.
    Size(u32, u32),
    Shutdown,
}

/// Handle owned by the capture session.
pub struct Input {
    tx: Sender<Msg>,
    wake: Arc<OwnedFd>,
    /// Set once the client has injected anything (health: a solid colour may
    /// then be user content).
    pub input_seen: Arc<AtomicBool>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

fn eventfd() -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn bump(fd: &OwnedFd) {
    let one: u64 = 1;
    unsafe { libc::write(fd.as_raw_fd(), (&one as *const u64).cast(), 8) };
}

fn drain(fd: &OwnedFd) {
    let mut v: u64 = 0;
    unsafe { libc::read(fd.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
}

impl Input {
    pub fn spawn() -> std::io::Result<Self> {
        let (tx, rx) = unbounded();
        let wake = Arc::new(eventfd()?);
        let input_seen = Arc::new(AtomicBool::new(false));
        let (w, seen) = (wake.clone(), input_seen.clone());
        let thread = std::thread::Builder::new()
            .name("kde-input".into())
            .spawn(move || run(rx, w, seen))?;
        Ok(Self {
            tx,
            wake,
            input_seen,
            thread: std::sync::Mutex::new(Some(thread)),
        })
    }

    fn send(&self, msg: Msg) {
        if self.tx.send(msg).is_ok() {
            bump(&self.wake);
        }
    }

    pub fn inject(&self, ev: InputEvent) {
        self.send(Msg::Input(ev));
    }

    pub fn connect(&self) {
        self.send(Msg::Connect);
    }

    /// The virtual output was created or resized to `width x height`.
    pub fn set_size(&self, width: u32, height: u32) {
        self.send(Msg::Size(width, height));
    }

    /// Release held keys, close EIS, join the thread. Idempotent.
    pub fn shutdown(&self) {
        let t = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(t) = t {
            self.send(Msg::Shutdown);
            let _ = t.join();
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One live EIS session: the D-Bus connection that owns it, the libei
/// context, and the device state.
struct Eis {
    dbus: zbus::blocking::Connection,
    cookie: i32,
    context: ei::Context,
    converter: EiEventConverter,
    devices: Vec<reis::event::Device>,
    resumed: HashSet<reis::event::Device>,
    emulating: HashSet<reis::event::Device>,
    sequence: u32,
    /// Keyboard state from the keymap KWin advertised.
    xkb: Option<KeymapInfo>,
    locks: Option<Locks>,
}

struct KeymapInfo {
    /// Unicode → (evdev keycode, needs Shift) for the active layout.
    chars: HashMap<char, (u32, bool)>,
    caps: Option<u32>,
    num: Option<u32>,
    layout: String,
}

fn connect_eis() -> anyhow::Result<Eis> {
    let dbus = zbus::blocking::Connection::session()?;
    let reply = dbus.call_method(
        Some("org.kde.KWin"),
        EIS_PATH,
        Some(EIS_IFACE),
        "connectToEIS",
        &(EIS_CAPS,),
    )?;
    let (fd, cookie): (zbus::zvariant::OwnedFd, i32) = reply.body().deserialize()?;
    let fd: OwnedFd = fd.into();
    let context = ei::Context::new(UnixStream::from(fd))?;
    let resp = reis::handshake::ei_handshake_blocking(
        &context,
        "wayrdp",
        ei::handshake::ContextType::Sender,
    )?;
    let converter = EiEventConverter::new(&context, resp);
    Ok(Eis {
        dbus,
        cookie,
        context,
        converter,
        devices: Vec::new(),
        resumed: HashSet::new(),
        emulating: HashSet::new(),
        sequence: 0,
        xkb: None,
        locks: None,
    })
}

fn now_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

fn compile_keymap(keymap: &reis::event::Keymap) -> Option<KeymapInfo> {
    if keymap.type_ != ei::keyboard::KeymapType::Xkb {
        return None;
    }
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let fd = keymap.fd.try_clone().ok()?;
    // Safety: the fd is a sealed memfd KWin created for exactly this keymap,
    // `size` bytes long.
    let km = unsafe {
        xkb::Keymap::new_from_fd(
            &ctx,
            fd,
            keymap.size as usize,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
    }
    .ok()??;
    let mut chars = HashMap::new();
    for kc in km.min_keycode().raw()..=km.max_keycode().raw() {
        for level in 0..2u32 {
            for sym in km.key_get_syms_by_level(xkb::Keycode::new(kc), 0, level) {
                let c = xkb::keysym_to_utf32(*sym);
                if let Some(ch) = char::from_u32(c).filter(|&ch| ch != '\0') {
                    chars
                        .entry(ch)
                        .or_insert((kc.saturating_sub(8), level == 1));
                }
            }
        }
    }
    let idx = |name: &str| {
        let i = km.mod_get_index(name);
        (i != xkb::MOD_INVALID).then_some(i)
    };
    Some(KeymapInfo {
        chars,
        caps: idx(xkb::MOD_NAME_CAPS),
        num: idx(xkb::MOD_NAME_NUM),
        layout: km.layout_get_name(0).to_string(),
    })
}

impl Eis {
    /// Read the socket and process every pending event. Err = hung up.
    fn dispatch(&mut self) -> anyhow::Result<()> {
        match self.context.read() {
            Ok(0) => anyhow::bail!("EIS socket closed"),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        self.process_buffered()
    }

    /// Process events already read into the context's buffer. Must run once
    /// right after the handshake: the handshake's read also pulls in the
    /// seat KWin sends straight after it, and those bytes never make the
    /// socket readable again, so waiting for poll() would never bind the
    /// seat and no device would ever appear (input silently dropped).
    fn process_buffered(&mut self) -> anyhow::Result<()> {
        while let Some(result) = self.context.pending_event() {
            match result {
                reis::PendingRequestResult::Request(ev) => self.converter.handle_event(ev)?,
                reis::PendingRequestResult::ParseError(e) => {
                    anyhow::bail!("EIS parse error: {e:?}")
                }
                reis::PendingRequestResult::InvalidObject(_) => {}
            }
        }
        while let Some(ev) = self.converter.next_event() {
            self.on_event(ev)?;
        }
        let _ = self.context.flush();
        Ok(())
    }

    fn on_event(&mut self, ev: EiEvent) -> anyhow::Result<()> {
        match ev {
            EiEvent::Disconnected(d) => {
                anyhow::bail!("EIS disconnected: {:?} {:?}", d.reason, d.explanation)
            }
            EiEvent::SeatAdded(s) => {
                s.seat.bind_capabilities(
                    DeviceCapability::PointerAbsolute
                        | DeviceCapability::Pointer
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Text,
                );
            }
            EiEvent::DeviceAdded(d) => {
                if let Some(km) = d.device.keymap() {
                    self.xkb = compile_keymap(km);
                    if let Some(info) = &self.xkb {
                        eprintln!("[backend-kde] input: guest keymap layout {:?}", info.layout);
                    }
                }
                eprintln!(
                    "[backend-kde] input: EIS device {:?} added, regions {:?}",
                    d.device.name(),
                    d.device.regions()
                );
                self.devices.push(d.device);
            }
            EiEvent::DeviceRemoved(d) => {
                self.devices.retain(|x| x != &d.device);
                self.resumed.remove(&d.device);
                self.emulating.remove(&d.device);
            }
            EiEvent::DeviceResumed(d) => {
                self.resumed.insert(d.device);
            }
            EiEvent::DevicePaused(d) => {
                self.resumed.remove(&d.device);
                self.emulating.remove(&d.device);
            }
            EiEvent::KeyboardModifiers(m) => {
                if let Some(info) = &self.xkb {
                    let bit = |i: Option<u32>| i.is_some_and(|i| m.locked & (1 << i) != 0);
                    self.locks = Some(Locks {
                        caps: bit(info.caps),
                        num: bit(info.num),
                        scroll: false,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A resumed device exposing `T`, preferring one with absolute pointer
    /// motion (KWin's absolute device also carries buttons and scroll).
    fn device_with<T: ei::Interface>(&mut self) -> Option<(reis::event::Device, T)> {
        let mut best: Option<(reis::event::Device, T)> = None;
        for d in &self.devices {
            if !self.resumed.contains(d) {
                continue;
            }
            let Some(iface) = d.interface::<T>() else {
                continue;
            };
            let abs = d.has_capability(DeviceCapability::PointerAbsolute);
            if best.is_none() || abs {
                best = Some((d.clone(), iface));
                if abs {
                    break;
                }
            }
        }
        let (dev, iface) = best?;
        if self.emulating.insert(dev.clone()) {
            let serial = self.converter.connection().serial();
            dev.device().start_emulating(serial, self.sequence);
            self.sequence = self.sequence.wrapping_add(1);
        }
        Some((dev, iface))
    }

    fn frame(&self, dev: &reis::event::Device) {
        dev.device()
            .frame(self.converter.connection().serial(), now_us());
    }

    fn key(&mut self, evdev: u32, pressed: bool) {
        let Some((dev, kbd)) = self.device_with::<ei::Keyboard>() else {
            return;
        };
        let state = if pressed {
            ei::keyboard::KeyState::Press
        } else {
            ei::keyboard::KeyState::Released
        };
        kbd.key(evdev, state);
        self.frame(&dev);
    }

    fn inject(
        &mut self,
        ev: &InputEvent,
        keys_down: &mut HashSet<u32>,
        session: (u32, u32),
        debug: bool,
    ) {
        match ev {
            InputEvent::PointerMotion { x, y } => {
                let Some((dev, abs)) = self.device_with::<ei::PointerAbsolute>() else {
                    return;
                };
                let regions: Vec<input_map::Region<'_>> = dev
                    .regions()
                    .iter()
                    .map(|r| {
                        (
                            r.x,
                            r.y,
                            r.width,
                            r.height,
                            r.scale as f64,
                            r.mapping_id.as_deref(),
                        )
                    })
                    .collect();
                let picked =
                    input_map::pick_region(&regions, crate::recovery::KSCREEN_NAME, session);
                let (px, py) = match picked.map(|i| regions[i]) {
                    Some((rx, ry, rw, rh, scale, _)) => {
                        input_map::region_position(*x, *y, (rx, ry, rw, rh, scale))
                    }
                    None => (*x as f32, *y as f32),
                };
                abs.motion_absolute(px, py);
                self.frame(&dev);
            }
            InputEvent::PointerButton { button, pressed } => {
                let Some((dev, btn)) = self.device_with::<ei::Button>() else {
                    return;
                };
                let state = if *pressed {
                    ei::button::ButtonState::Press
                } else {
                    ei::button::ButtonState::Released
                };
                btn.button(input_map::button_code(*button), state);
                self.frame(&dev);
            }
            InputEvent::PointerAxis { dx, dy } => {
                let Some((dev, scroll)) = self.device_with::<ei::Scroll>() else {
                    return;
                };
                let (sx, sy) = input_map::scroll_discrete(*dx, *dy);
                scroll.scroll_discrete(sx, sy);
                self.frame(&dev);
            }
            InputEvent::Key { scancode, pressed } => {
                let key = input_keymap::rdp_scancode_to_evdev(*scancode);
                if key == 0 {
                    if debug {
                        eprintln!("[backend-kde] input: unmapped scancode {scancode:#x}");
                    }
                    return;
                }
                // mstsc floods modifier repeats; a press of an already-down
                // key is swallowed so KWin doesn't see autorepeat chords.
                if *pressed {
                    if !keys_down.insert(key) {
                        return;
                    }
                } else {
                    keys_down.remove(&key);
                }
                if debug {
                    eprintln!("[backend-kde] input: key {key} pressed={pressed}");
                }
                self.key(key, *pressed);
            }
            InputEvent::Unicode(ch) => {
                if let Some((dev, text)) = self.device_with::<ei::Text>() {
                    text.utf8(&ch.to_string());
                    self.frame(&dev);
                    return;
                }
                let Some(&(code, shift)) = self.xkb.as_ref().and_then(|k| k.chars.get(ch)) else {
                    if debug {
                        eprintln!(
                            "[backend-kde] input: no key produces {ch:?} on the guest layout"
                        );
                    }
                    return;
                };
                const KEY_LEFTSHIFT: u32 = 42;
                if shift {
                    self.key(KEY_LEFTSHIFT, true);
                }
                self.key(code, true);
                self.key(code, false);
                if shift {
                    self.key(KEY_LEFTSHIFT, false);
                }
            }
            InputEvent::LockKeys { lock_bits } => {
                // Scroll lock is no xkb modifier: take the client's word.
                let current = self.locks.map(|l| Locks {
                    scroll: lock_bits & input_map::SYNC_SCROLL != 0,
                    ..l
                });
                for key in input_map::lock_taps(current, *lock_bits) {
                    self.key(key, true);
                    self.key(key, false);
                }
            }
        }
    }

    /// Release held keys, stop emulating, and give the context back.
    fn close(mut self, keys_down: &mut HashSet<u32>) {
        for key in std::mem::take(keys_down) {
            self.key(key, false);
        }
        let serial = self.converter.connection().serial();
        for d in &self.emulating {
            d.device().stop_emulating(serial);
        }
        self.converter.connection().connection().disconnect();
        let _ = self.context.flush();
        let _ = self.dbus.call_method(
            Some("org.kde.KWin"),
            EIS_PATH,
            Some(EIS_IFACE),
            "disconnect",
            &(self.cookie,),
        );
    }
}

fn run(rx: Receiver<Msg>, wake: Arc<OwnedFd>, input_seen: Arc<AtomicBool>) {
    let debug = std::env::var_os("WAYRDP_DEBUG_INPUT").is_some();
    let mut eis: Option<Eis> = None;
    let mut want = false;
    let mut retry: Option<(Instant, usize)> = None;
    let mut keys_down = HashSet::new();
    let mut pending: Vec<InputEvent> = Vec::new();
    let mut session = (0u32, 0u32);

    'main: loop {
        // (Re)connect when asked, with backoff after failures.
        if want && eis.is_none() && retry.is_none_or(|(at, _)| Instant::now() >= at) {
            let attempt = retry.map_or(0, |(_, n)| n);
            match connect_eis() {
                Ok(mut e) => {
                    eprintln!("[backend-kde] input: EIS connected (cookie {})", e.cookie);
                    match e.process_buffered() {
                        Ok(()) => {
                            eis = Some(e);
                            retry = None;
                        }
                        Err(err) => {
                            eprintln!("[backend-kde] input: {err:#}; reconnecting");
                            retry = Some((Instant::now() + Duration::from_secs(1), attempt + 1));
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[backend-kde] input: EIS connect failed: {e:#}");
                    match BACKOFF.get(attempt) {
                        Some(&s) => {
                            retry = Some((Instant::now() + Duration::from_secs(s), attempt + 1))
                        }
                        None => {
                            eprintln!("[backend-kde] input: giving up on EIS; input is dropped");
                            want = false;
                        }
                    }
                }
            }
        }

        let mut fds = vec![libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if let Some(e) = &eis {
            fds.push(libc::pollfd {
                fd: e.context.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let timeout = match retry {
            Some((at, _)) => at
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(5_000) as i32,
            None => 1_000,
        };
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };

        if fds.get(1).is_some_and(|f| f.revents != 0) {
            if let Err(e) = eis.as_mut().map_or(Ok(()), Eis::dispatch) {
                eprintln!("[backend-kde] input: {e:#}; reconnecting");
                eis = None;
                keys_down.clear();
                retry = Some((Instant::now() + Duration::from_secs(1), 0));
            }
        }

        if fds[0].revents != 0 {
            drain(&wake);
        }
        for msg in rx.try_iter() {
            match msg {
                Msg::Shutdown => break 'main,
                Msg::Connect => {
                    want = true;
                    retry = None;
                }
                Msg::Size(w, h) => session = (w, h),
                Msg::Input(ev) => pending.push(ev),
            }
        }
        if !pending.is_empty() {
            input_seen.store(true, Ordering::Relaxed);
            let batch = input_map::coalesce_motion(std::mem::take(&mut pending));
            if let Some(e) = eis.as_mut() {
                for ev in &batch {
                    e.inject(ev, &mut keys_down, session, debug);
                }
                let _ = e.context.flush();
            }
        }
    }
    if let Some(e) = eis.take() {
        e.close(&mut keys_down);
    }
}
