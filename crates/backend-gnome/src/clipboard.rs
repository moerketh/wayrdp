//! GNOME clipboard: the selection API of Mutter's RemoteDesktop session.
//!
//! Mutter 50 serves no data-control protocol (neither ext nor wlr), so the
//! Wayland clipboard used on Hyprland and KDE has nothing to bind. The
//! RemoteDesktop session has its own clipboard instead: `EnableClipboard`,
//! `SetSelection`, `SelectionRead`, `SelectionWrite` / `SelectionWriteDone`,
//! and the `SelectionOwnerChanged` / `SelectionTransfer` signals.
//!
//! The clipboard runs its own RemoteDesktop session, started without a
//! ScreenCast (no monitor, no stream). rdp-core opens the clipboard at
//! connect time, before the capture session exists, and the capture side
//! recreates its Mutter session on heals; a separate session keeps the
//! clipboard out of both. Mutter stops the session when our connection
//! goes away, so nothing outlives the process.
//!
//! Behaviour seen on Mutter 50.5 (2026-11-22):
//! - `EnableClipboard` reports the current selection right away;
//! - our own `SetSelection` echoes back with `session-is-owner = true`;
//! - Mutter's clipboard manager reads new text and image selections at once
//!   (so it survives the source), which asks us for remote text on every
//!   client copy. It never reads file lists;
//! - the fd `SelectionRead` returns is non-blocking;
//! - `CreateSession` fails while the screen is locked, so we retry.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read as _, Write};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use crossbeam_channel::Sender;
use desk_backend::{BackendError, Clipboard, ClipboardEvent, Result};
use futures_util::StreamExt as _;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use crate::mutter::{RD, RD_PATH, RD_SESSION};

/// Between connect attempts (lock screen, gnome-shell restarting).
const RETRY: Duration = Duration::from_secs(2);
/// How long `Drop` waits for the clipboard thread.
const STOP_WAIT: Duration = Duration::from_secs(3);

enum Cmd {
    Offer(Vec<String>),
    Shutdown,
}

/// The live session, for calls made from rdp-core's threads.
#[derive(Clone)]
struct Link {
    conn: zbus::blocking::Connection,
    rd: OwnedObjectPath,
}

impl Link {
    fn call(
        &self,
        method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> anyhow::Result<zbus::Message> {
        self.conn
            .call_method(Some(RD), self.rd.as_str(), Some(RD_SESSION), method, body)
            .with_context(|| format!("RemoteDesktop.Session.{method}"))
    }
}

type Slot = Arc<Mutex<Option<Link>>>;

fn current(slot: &Slot) -> Option<Link> {
    slot.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn set(slot: &Slot, link: Option<Link>) {
    *slot.lock().unwrap_or_else(|p| p.into_inner()) = link;
}

pub struct MutterClipboard {
    link: Slot,
    cmd: Option<UnboundedSender<Cmd>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MutterClipboard {
    /// No I/O here (desk-backend contract): the session opens in `start`.
    pub fn new() -> Self {
        Self {
            link: Slot::default(),
            cmd: None,
            thread: None,
        }
    }
}

impl Default for MutterClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MutterClipboard {
    fn drop(&mut self) {
        if let Some(tx) = self.cmd.take() {
            let _ = tx.send(Cmd::Shutdown);
        }
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + STOP_WAIT;
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                eprintln!("[backend-gnome] clipboard: thread still running; detaching");
            }
        }
    }
}

impl Clipboard for MutterClipboard {
    /// Never fails on a locked screen: the thread keeps trying to connect,
    /// and until it does, reads fail and offers wait.
    fn start(&mut self, tx: Sender<ClipboardEvent>) -> Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let link = self.link.clone();
        let thread = std::thread::Builder::new()
            .name("gnome-clipboard".into())
            .spawn(move || run(tx, cmd_rx, link))
            .map_err(|e| BackendError::Other(e.into()))?;
        self.cmd = Some(cmd_tx);
        self.thread = Some(thread);
        Ok(())
    }

    fn offer_remote(&self, mimes: Vec<String>) -> Result<()> {
        let tx = self
            .cmd
            .as_ref()
            .ok_or_else(|| BackendError::Unavailable("clipboard not started".into()))?;
        tx.send(Cmd::Offer(mimes))
            .map_err(|_| BackendError::Unavailable("clipboard thread gone".into()))
    }

    fn read_local(&self, mime: &str, timeout: Duration, max_bytes: usize) -> Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let link = current(&self.link).ok_or_else(|| {
            BackendError::Unavailable("no GNOME remote-desktop session for the clipboard".into())
        })?;
        let reply = link
            .call("SelectionRead", &(mime,))
            .map_err(|e| BackendError::Unavailable(format!("{e:#}")))?;
        let (fd,): (zbus::zvariant::OwnedFd,) = reply
            .body()
            .deserialize()
            .map_err(|e| BackendError::Other(e.into()))?;
        read_until_eof(fd.into(), deadline, max_bytes)
            .map_err(|e| BackendError::Other(anyhow::anyhow!("reading {mime}: {e}")))
    }
}

/// Thread body: connect, serve, reconnect until shut down.
fn run(events: Sender<ClipboardEvent>, mut cmds: UnboundedReceiver<Cmd>, link: Slot) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[backend-gnome] clipboard: no tokio runtime: {e}");
            return;
        }
    };
    rt.block_on(async move {
        // The client's latest offer: replayed when a session (re)opens,
        // dropped once a local application takes the selection.
        let mut offer: Option<Vec<String>> = None;
        let mut logged_failure = false;
        loop {
            let mut session = match Session::open(offer.as_deref()).await {
                Ok(s) => {
                    eprintln!(
                        "[backend-gnome] clipboard: enabled on session {}",
                        s.label()
                    );
                    logged_failure = false;
                    s
                }
                Err(e) => {
                    if !logged_failure {
                        logged_failure = true;
                        eprintln!(
                            "[backend-gnome] clipboard: no remote-desktop session yet ({e:#}); \
                             retrying every {}s",
                            RETRY.as_secs()
                        );
                    }
                    if wait_or_shutdown(&mut cmds, &mut offer, RETRY).await {
                        return;
                    }
                    continue;
                }
            };
            set(&link, Some(session.link()));
            let end = session.serve(&events, &mut cmds, &mut offer).await;
            set(&link, None);
            session.stop().await;
            match end {
                End::Shutdown => return,
                End::Closed => {
                    eprintln!("[backend-gnome] clipboard: Mutter closed the session; reopening");
                    if wait_or_shutdown(&mut cmds, &mut offer, RETRY).await {
                        return;
                    }
                }
            }
        }
    });
}

/// Sleep for `d`, keeping the newest offer. `true` = shut down.
async fn wait_or_shutdown(
    cmds: &mut UnboundedReceiver<Cmd>,
    offer: &mut Option<Vec<String>>,
    d: Duration,
) -> bool {
    let until = tokio::time::Instant::now() + d;
    loop {
        tokio::select! {
            c = cmds.recv() => match c {
                Some(Cmd::Offer(m)) => *offer = Some(m),
                Some(Cmd::Shutdown) | None => return true,
            },
            _ = tokio::time::sleep_until(until) => return false,
        }
    }
}

enum End {
    Shutdown,
    Closed,
}

struct Session {
    conn: Connection,
    rd: OwnedObjectPath,
    owner_changed: MessageStream,
    transfer: MessageStream,
    closed: MessageStream,
}

async fn signal(conn: &Connection, member: &str, path: &str) -> anyhow::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(RD_SESSION)?
        .member(member.to_string())?
        .path(path.to_string())?
        .build();
    Ok(MessageStream::for_match_rule(rule, conn, None).await?)
}

fn mime_opts(mimes: &[String]) -> HashMap<&'static str, Value<'static>> {
    HashMap::from([("mime-types", Value::from(mimes.to_vec()))])
}

impl Session {
    /// A RemoteDesktop session with no ScreenCast, started, clipboard on.
    /// `offer` (the client's current selection) is set atomically with it.
    async fn open(offer: Option<&[String]>) -> anyhow::Result<Self> {
        let conn = Connection::session().await.context("session bus")?;
        let reply = conn
            .call_method(Some(RD), RD_PATH, Some(RD), "CreateSession", &())
            .await
            .context("RemoteDesktop.CreateSession")?;
        let (rd,): (OwnedObjectPath,) = reply.body().deserialize()?;
        // On error the connection drops and Mutter stops the session.
        let s = Self::subscribe(conn, rd).await?;
        if let Err(e) = s.enable(offer).await {
            s.stop().await;
            return Err(e);
        }
        Ok(s)
    }

    async fn subscribe(conn: Connection, rd: OwnedObjectPath) -> anyhow::Result<Self> {
        let p = rd.as_str();
        let owner_changed = signal(&conn, "SelectionOwnerChanged", p).await?;
        let transfer = signal(&conn, "SelectionTransfer", p).await?;
        let closed = signal(&conn, "Closed", p).await?;
        Ok(Self {
            conn,
            rd,
            owner_changed,
            transfer,
            closed,
        })
    }

    async fn call<B>(&self, method: &str, body: &B) -> anyhow::Result<zbus::Message>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.conn
            .call_method(Some(RD), self.rd.as_str(), Some(RD_SESSION), method, body)
            .await
            .with_context(|| format!("RemoteDesktop.Session.{method}"))
    }

    async fn enable(&self, offer: Option<&[String]>) -> anyhow::Result<()> {
        self.call("Start", &()).await?;
        let opts = offer.map(mime_opts).unwrap_or_default();
        self.call("EnableClipboard", &(opts,)).await?;
        Ok(())
    }

    async fn stop(&self) {
        let _ = self.call("Stop", &()).await;
    }

    fn link(&self) -> Link {
        Link {
            conn: zbus::blocking::Connection::from(self.conn.clone()),
            rd: self.rd.clone(),
        }
    }

    fn label(&self) -> &str {
        self.rd.as_str().rsplit('/').next().unwrap_or("?")
    }

    async fn serve(
        &mut self,
        events: &Sender<ClipboardEvent>,
        cmds: &mut UnboundedReceiver<Cmd>,
        offer: &mut Option<Vec<String>>,
    ) -> End {
        loop {
            tokio::select! {
                c = cmds.recv() => match c {
                    Some(Cmd::Offer(mimes)) => {
                        if let Err(e) = self.call("SetSelection", &(mime_opts(&mimes),)).await {
                            eprintln!("[backend-gnome] clipboard: {e:#}");
                        }
                        *offer = Some(mimes);
                    }
                    Some(Cmd::Shutdown) | None => return End::Shutdown,
                },
                m = self.owner_changed.next() => {
                    let Some(Ok(m)) = m else { return End::Closed };
                    let Ok((opts,)) = m.body().deserialize::<(HashMap<String, OwnedValue>,)>() else {
                        continue;
                    };
                    let (mimes, ours) = owner_change(&opts);
                    // Our own SetSelection coming back: the client knows.
                    if ours {
                        continue;
                    }
                    *offer = None;
                    eprintln!(
                        "[backend-gnome] clipboard: local selection, {} type(s)",
                        mimes.len()
                    );
                    let _ = events.send(ClipboardEvent::LocalSelection { mimes });
                }
                m = self.transfer.next() => {
                    let Some(Ok(m)) = m else { return End::Closed };
                    let Ok((mime, serial)) = m.body().deserialize::<(String, u32)>() else {
                        continue;
                    };
                    self.transfer_to_local(events, mime, serial).await;
                }
                m = self.closed.next() => {
                    if m.is_some() {
                        return End::Closed;
                    }
                }
            }
        }
    }

    /// A local application pastes what the client offered: get the fd
    /// Mutter hands the data through and let rdp-core fill it.
    async fn transfer_to_local(&self, events: &Sender<ClipboardEvent>, mime: String, serial: u32) {
        let fd = match self.call("SelectionWrite", &(serial,)).await.and_then(|r| {
            let (fd,): (zbus::zvariant::OwnedFd,) = r.body().deserialize()?;
            Ok(OwnedFd::from(fd))
        }) {
            Ok(fd) => fd,
            Err(e) => {
                // A newer SetSelection (another client copy) cancels the
                // transfers still pending; that one is already answered.
                if format!("{e:#}").contains("doesn't match any transfer") {
                    eprintln!("[backend-gnome] clipboard: transfer {serial} superseded");
                } else {
                    eprintln!("[backend-gnome] clipboard: {e:#}");
                    let _ = self.call("SelectionWriteDone", &(serial, false)).await;
                }
                return;
            }
        };
        set_blocking(&fd);
        let sink = Sink {
            file: Some(File::from(fd)),
            serial,
            link: self.link(),
            wrote: false,
            failed: false,
        };
        if let Err(crossbeam_channel::SendError(ev)) = events.send(ClipboardEvent::SendRequest {
            mime,
            sink: Box::new(sink),
        }) {
            drop(ev); // the sink's Drop answers Mutter
        }
    }
}

/// `SelectionOwnerChanged` options → (mime types, session-is-owner).
///
/// Mutter 50 sends `mime-types` as a one-element tuple, `(as)`, although
/// its XML documents `as`; accept both.
fn owner_change(opts: &HashMap<String, OwnedValue>) -> (Vec<String>, bool) {
    let mimes = opts
        .get("mime-types")
        .and_then(|v| {
            let v = v.try_clone().ok()?;
            if let Ok((list,)) = <(Vec<String>,)>::try_from(v.try_clone().ok()?) {
                return Some(list);
            }
            Vec::<String>::try_from(v).ok()
        })
        .unwrap_or_default();
    let ours = opts
        .get("session-is-owner")
        .and_then(|v| bool::try_from(&**v).ok())
        .unwrap_or(false);
    (mimes, ours)
}

/// The pipe a `SelectionTransfer` is answered through. Closing it ends the
/// transfer; Mutter then wants `SelectionWriteDone` with the outcome.
struct Sink {
    file: Option<File>,
    serial: u32,
    link: Link,
    wrote: bool,
    failed: bool,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        match file.write(buf) {
            Ok(n) => {
                self.wrote |= n > 0;
                Ok(n)
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        drop(self.file.take());
        // Nothing written = rdp-core could not get the data from the client.
        let ok = self.wrote && !self.failed;
        if let Err(e) = self.link.call("SelectionWriteDone", &(self.serial, ok)) {
            eprintln!("[backend-gnome] clipboard: {e:#}");
        }
    }
}

fn set_blocking(fd: &OwnedFd) {
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}

/// Drain the (non-blocking) read end Mutter returns, bounded in time and
/// size: the source application writes at its own pace and may hang.
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
        let ms = left.as_millis().min(i32::MAX as u128) as i32;
        if unsafe { libc::poll(pfd.as_mut_ptr(), 1, ms) } <= 0 {
            continue;
        }
        let n = match file.read(&mut buf) {
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                continue
            }
            Err(e) => return Err(e),
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape Mutter 50 sends: `mime-types` is `(as)`.
    fn opts(mimes: &[&str], ours: Option<bool>) -> HashMap<String, OwnedValue> {
        let mut m = HashMap::new();
        let list: Vec<String> = mimes.iter().map(|s| s.to_string()).collect();
        m.insert(
            "mime-types".to_string(),
            OwnedValue::try_from(Value::from((list,))).unwrap(),
        );
        if let Some(o) = ours {
            m.insert(
                "session-is-owner".to_string(),
                OwnedValue::try_from(Value::from(o)).unwrap(),
            );
        }
        m
    }

    #[test]
    fn local_owner_reports_mimes() {
        let (mimes, ours) =
            owner_change(&opts(&["text/plain;charset=utf-8", "TARGETS"], Some(false)));
        assert_eq!(mimes, ["text/plain;charset=utf-8", "TARGETS"]);
        assert!(!ours);
    }

    #[test]
    fn plain_string_array_is_accepted_too() {
        let list = vec!["text/plain".to_string()];
        let m = HashMap::from([(
            "mime-types".to_string(),
            OwnedValue::try_from(Value::from(list)).unwrap(),
        )]);
        assert_eq!(owner_change(&m).0, ["text/plain"]);
    }

    #[test]
    fn own_echo_is_flagged() {
        let (_, ours) = owner_change(&opts(&["text/plain"], Some(true)));
        assert!(ours);
    }

    #[test]
    fn missing_keys_mean_cleared_and_not_ours() {
        let (mimes, ours) = owner_change(&HashMap::new());
        assert!(mimes.is_empty());
        assert!(!ours);
        let (_, ours) = owner_change(&opts(&[], None));
        assert!(!ours);
    }

    #[test]
    fn read_until_eof_bounds_size() {
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) },
            0
        );
        use std::os::fd::FromRawFd as _;
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut w = File::from(w);
        w.write_all(b"hello world").unwrap();
        drop(w);
        let deadline = Instant::now() + Duration::from_secs(1);
        let err = read_until_eof(r, deadline, 5).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::OutOfMemory);
    }

    #[test]
    fn read_until_eof_reads_all() {
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) },
            0
        );
        use std::os::fd::FromRawFd as _;
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut w = File::from(w);
        w.write_all(b"hello").unwrap();
        drop(w);
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(read_until_eof(r, deadline, 64).unwrap(), b"hello");
    }
}
