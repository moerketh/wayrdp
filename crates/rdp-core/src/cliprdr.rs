//! Clipboard redirection (MS-RDPECLIP) between the RDP client and the
//! session's Wayland clipboard: text and files, both directions.
//!
//! Threads:
//! - FreeRDP's cliprdr channel thread runs the `Client*` callbacks. They
//!   never block: requests are queued to the outbound worker, responses are
//!   handed to whichever inbound fetch is waiting for them.
//! - The outbound worker (guest → host) announces local selections and
//!   serves the client's data / file-range requests from the local clipboard.
//! - The event thread receives Wayland clipboard events; each paste of
//!   remote data (host → guest) runs on its own short-lived fetch thread,
//!   serialized by `fetch_lock` because a cliprdr data response carries no
//!   format id — only one request may be in flight.
//!
//! Every `Server*` send goes through the `gate`: teardown clears it before
//! Stop/free, so a late worker send can never touch a freed context.

use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use desk_backend::{Clipboard, ClipboardEvent};
use freerdp_sys::*;

use crate::clipformat::{self as fmt, FileDescriptor, LocalEntry};

const CB_RESPONSE_OK: u16 = 0x0001;
const FILECONTENTS_SIZE: u32 = 0x0000_0001;
const FILECONTENTS_RANGE: u32 = 0x0000_0002;

/// Client answers to our data requests; mstsc answers in well under this.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Local selection reads (guest → host); a stuck local source fails the
/// request instead of hanging the outbound worker.
const LOCAL_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TEXT_BYTES: usize = 64 * 1024 * 1024;
const MAX_LOCAL_ENTRIES: usize = 10_000;
/// Entries accepted from a client FileGroupDescriptorW (host → guest
/// downloads). Mirrors MAX_LOCAL_ENTRIES on the serving side.
const MAX_REMOTE_ENTRIES: usize = 10_000;
/// Default aggregate byte budget for one host → guest download
/// announcement. The client controls both the declared per-entry sizes and
/// every response body, so without a budget a malicious peer can fill the
/// user's disk. Operators who legitimately paste larger trees raise it with
/// WAYRDP_DOWNLOAD_BUDGET_MB.
const DEFAULT_DOWNLOAD_BUDGET_MB: u64 = 2 * 1024;

/// Parsed once: WAYRDP_DOWNLOAD_BUDGET_MB, clamped to 1..=(1<<20) MiB.
fn download_budget() -> u64 {
    static BUDGET: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        let mb = std::env::var("WAYRDP_DOWNLOAD_BUDGET_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_DOWNLOAD_BUDGET_MB)
            .clamp(1, 1 << 20);
        mb * 1024 * 1024
    })
}
/// FILECONTENTS range size per request (host → guest downloads).
const RANGE_CHUNK: u32 = 1024 * 1024;
/// Largest range we serve per client request (guest → host).
const MAX_SERVE_CHUNK: u32 = 8 * 1024 * 1024;

struct CtxPtr(*mut CliprdrServerContext);
// The context is only dereferenced under the gate lock, while it is alive.
unsafe impl Send for CtxPtr {}

enum OutJob {
    /// Client capabilities arrived: announcements may go out now.
    Ready,
    Local(Vec<String>),
    Data(u32),
    File {
        stream_id: u32,
        list_index: u32,
        flags: u32,
        position: u64,
        cb: u32,
    },
}

#[derive(Default)]
struct Outbound {
    ready: bool,
    /// Latest local selection not yet announced (held until `ready`).
    pending: Option<Vec<String>>,
    text_mime: Option<String>,
    files_mime: Option<&'static str>,
    /// Snapshot served by FileContents, taken when the client fetched the
    /// FileGroupDescriptorW (list_index indexes into it).
    entries: Vec<LocalEntry>,
}

#[derive(Default)]
struct RemoteOffer {
    /// Bumped on every client format list; caches are per announcement.
    seq: u64,
    text_format: Option<u32>,
    files_format: Option<u32>,
    /// Downloaded copy of the current announcement's files.
    files_cache: Option<(u64, Vec<PathBuf>)>,
}

struct Shared {
    gate: Mutex<Option<CtxPtr>>,
    clipboard: Mutex<Option<Arc<dyn Clipboard>>>,
    out_tx: Mutex<Option<Sender<OutJob>>>,
    remote: Mutex<RemoteOffer>,
    /// Staging directories this peer created (host → guest downloads);
    /// drained on the next download and removed on Drop. Tracked per
    /// instance so teardown never deletes a concurrent peer's downloads
    /// (the staging root is shared per user account).
    staging: Mutex<Vec<PathBuf>>,
    /// Waiter for the next ClientFormatDataResponse (None = failed).
    data_waiter: Mutex<Option<Sender<Option<Vec<u8>>>>>,
    file_waiters: Mutex<HashMap<u32, Sender<Option<Vec<u8>>>>>,
    fetch_lock: Mutex<()>,
    next_stream: AtomicU32,
}

impl Shared {
    fn with_ctx(&self, f: impl FnOnce(*mut CliprdrServerContext) -> UINT) -> bool {
        let gate = self.gate.lock().unwrap();
        match gate.as_ref() {
            Some(c) => f(c.0) == 0,
            None => false,
        }
    }

    fn clipboard(&self) -> Option<Arc<dyn Clipboard>> {
        self.clipboard.lock().unwrap().clone()
    }

    fn queue(&self, job: OutJob) {
        if let Some(tx) = self.out_tx.lock().unwrap().as_ref() {
            let _ = tx.send(job);
        }
    }
}

/// One peer's clipboard channel. Dropping it tears everything down.
pub struct Cliprdr {
    shared: Arc<Shared>,
    ctx: *mut CliprdrServerContext,
    custom: *const Shared,
}

impl Cliprdr {
    /// Create and start the cliprdr channel for this peer. `None` (logged)
    /// when the channel or the local clipboard can't come up — the session
    /// runs without clipboard then.
    ///
    /// # Safety
    /// `vcm` and `rdpctx` must be the peer's live VCM and context; the
    /// returned value must be dropped before the VCM is closed.
    pub unsafe fn start(
        vcm: HANDLE,
        rdpctx: *mut rdpContext,
        mut clipboard: Box<dyn Clipboard>,
    ) -> Option<Box<Cliprdr>> {
        let (ev_tx, ev_rx) = unbounded::<ClipboardEvent>();
        if let Err(e) = clipboard.start(ev_tx) {
            eprintln!("[cliprdr] local clipboard unavailable: {e}; no clipboard redirection");
            return None;
        }
        let ctx = wayrdp_cliprdr_new(vcm, rdpctx as *mut _);
        if ctx.is_null() {
            eprintln!("[cliprdr] cliprdr_server_context_new failed");
            return None;
        }
        let (out_tx, out_rx) = unbounded::<OutJob>();
        let shared = Arc::new(Shared {
            gate: Mutex::new(Some(CtxPtr(ctx))),
            clipboard: Mutex::new(Some(Arc::from(clipboard))),
            out_tx: Mutex::new(Some(out_tx)),
            remote: Mutex::new(RemoteOffer::default()),
            staging: Mutex::new(Vec::new()),
            data_waiter: Mutex::new(None),
            file_waiters: Mutex::new(HashMap::new()),
            fetch_lock: Mutex::new(()),
            next_stream: AtomicU32::new(1),
        });
        let custom = Arc::into_raw(Arc::clone(&shared));
        (*ctx).custom = custom as *mut _;
        (*ctx).ClientCapabilities = Some(on_client_capabilities);
        (*ctx).ClientFormatList = Some(on_client_format_list);
        (*ctx).ClientFormatListResponse = Some(on_client_format_list_response);
        (*ctx).ClientFormatDataRequest = Some(on_client_format_data_request);
        (*ctx).ClientFormatDataResponse = Some(on_client_format_data_response);
        (*ctx).ClientFileContentsRequest = Some(on_client_file_contents_request);
        (*ctx).ClientFileContentsResponse = Some(on_client_file_contents_response);

        let w = Arc::clone(&shared);
        let _ = std::thread::Builder::new()
            .name("wayrdp-clip-out".into())
            .spawn(move || outbound_worker(w, out_rx));
        let w = Arc::clone(&shared);
        let _ = std::thread::Builder::new()
            .name("wayrdp-clip-ev".into())
            .spawn(move || event_loop(w, ev_rx));

        // Reap trees abandoned by earlier runs (teardown leaves staging in
        // place so a disconnect can never delete files mid-read; see
        // sweep_stale_staging). Only trees older than a day go.
        sweep_stale_staging();

        let rc = wayrdp_cliprdr_start(ctx);
        let me = Box::new(Cliprdr {
            shared,
            ctx,
            custom,
        });
        if rc != 0 {
            eprintln!("[cliprdr] Start failed (0x{rc:x}); no clipboard redirection");
            return None; // Drop tears down the workers and the context
        }
        eprintln!("[cliprdr] started (text + files, both directions)");
        Some(me)
    }
}

impl Drop for Cliprdr {
    fn drop(&mut self) {
        // Close the send gate first: workers see a dead channel from here on.
        *self.shared.gate.lock().unwrap() = None;
        unsafe {
            let _ = wayrdp_cliprdr_stop(self.ctx);
            cliprdr_server_context_free(self.ctx);
            drop(Arc::from_raw(self.custom));
        }
        // Wake every waiter, stop the outbound worker, and drop the local
        // clipboard: its thread exits, which ends the event loop.
        self.shared.data_waiter.lock().unwrap().take();
        self.shared.file_waiters.lock().unwrap().clear();
        self.shared.out_tx.lock().unwrap().take();
        self.shared.clipboard.lock().unwrap().take();
        // Staging is deliberately left on disk: a disconnect mid-paste must
        // not delete files a local app may still be reading from the
        // staging path. The next download sweeps this peer's superseded
        // trees; a later channel start reaps anything older than a day.
        self.shared.staging.lock().unwrap().clear();
    }
}

// ── channel-thread callbacks ────────────────────────────────────────────

unsafe fn shared_of<'a>(c: *mut CliprdrServerContext) -> Option<&'a Shared> {
    if c.is_null() || (*c).custom.is_null() {
        return None;
    }
    Some(&*((*c).custom as *const Shared))
}

/// Panic guard + common prologue for every callback.
unsafe fn callback(c: *mut CliprdrServerContext, f: impl FnOnce(&Shared)) -> UINT {
    let Some(s) = shared_of(c) else { return 0 };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(s)));
    0
}

unsafe extern "C" fn on_client_capabilities(
    c: *mut CliprdrServerContext,
    _caps: *const CLIPRDR_CAPABILITIES,
) -> UINT {
    callback(c, |s| s.queue(OutJob::Ready))
}

unsafe extern "C" fn on_client_format_list(
    c: *mut CliprdrServerContext,
    list: *const CLIPRDR_FORMAT_LIST,
) -> UINT {
    callback(c, |s| {
        let mut text = None;
        let mut files = None;
        if !list.is_null() && !(*list).formats.is_null() {
            let formats = std::slice::from_raw_parts((*list).formats, (*list).numFormats as usize);
            for f in formats {
                let name = if f.formatName.is_null() {
                    None
                } else {
                    Some(CStr::from_ptr(f.formatName).to_string_lossy())
                };
                match (f.formatId, name.as_deref()) {
                    (fmt::CF_UNICODETEXT, _) => text = Some(fmt::CF_UNICODETEXT),
                    (fmt::CF_TEXT | fmt::CF_OEMTEXT, _) if text.is_none() => {
                        text = Some(f.formatId)
                    }
                    (id, Some(fmt::FILEGROUPDESCRIPTORW_NAME)) => files = Some(id),
                    _ => {}
                }
            }
        }
        {
            let mut r = s.remote.lock().unwrap();
            r.seq += 1;
            r.text_format = text;
            r.files_format = files;
            r.files_cache = None;
        }
        s.with_ctx(|c| wayrdp_cliprdr_send_format_list_response(c, 1));

        let mut mimes: Vec<String> = Vec::new();
        if files.is_some() {
            mimes.push(fmt::URI_LIST_MIME.into());
            mimes.push(fmt::GNOME_COPIED_FILES_MIME.into());
        }
        if text.is_some() {
            mimes.extend(fmt::TEXT_MIMES.iter().map(|m| m.to_string()));
        }
        if !mimes.is_empty() {
            if let Some(cb) = s.clipboard() {
                if let Err(e) = cb.offer_remote(mimes) {
                    eprintln!("[cliprdr] offering remote selection failed: {e}");
                }
            }
        }
    })
}

unsafe extern "C" fn on_client_format_list_response(
    c: *mut CliprdrServerContext,
    _r: *const CLIPRDR_FORMAT_LIST_RESPONSE,
) -> UINT {
    callback(c, |_| {})
}

unsafe extern "C" fn on_client_format_data_request(
    c: *mut CliprdrServerContext,
    r: *const CLIPRDR_FORMAT_DATA_REQUEST,
) -> UINT {
    callback(c, |s| {
        if !r.is_null() {
            s.queue(OutJob::Data((*r).requestedFormatId));
        }
    })
}

unsafe extern "C" fn on_client_format_data_response(
    c: *mut CliprdrServerContext,
    r: *const CLIPRDR_FORMAT_DATA_RESPONSE,
) -> UINT {
    callback(c, |s| {
        let payload = response_bytes(
            r.as_ref()
                .map(|r| (r.common.msgFlags, r.requestedFormatData, r.common.dataLen)),
        );
        if let Some(w) = s.data_waiter.lock().unwrap().take() {
            let _ = w.send(payload);
        }
    })
}

unsafe extern "C" fn on_client_file_contents_request(
    c: *mut CliprdrServerContext,
    r: *const CLIPRDR_FILE_CONTENTS_REQUEST,
) -> UINT {
    callback(c, |s| {
        if let Some(r) = r.as_ref() {
            s.queue(OutJob::File {
                stream_id: r.streamId,
                list_index: r.listIndex,
                flags: r.dwFlags,
                position: (u64::from(r.nPositionHigh) << 32) | u64::from(r.nPositionLow),
                cb: r.cbRequested,
            });
        }
    })
}

unsafe extern "C" fn on_client_file_contents_response(
    c: *mut CliprdrServerContext,
    r: *const CLIPRDR_FILE_CONTENTS_RESPONSE,
) -> UINT {
    callback(c, |s| {
        let Some(r) = r.as_ref() else { return };
        let payload = response_bytes(Some((r.common.msgFlags, r.requestedData, r.cbRequested)));
        if let Some(w) = s.file_waiters.lock().unwrap().remove(&r.streamId) {
            let _ = w.send(payload);
        }
    })
}

/// Copy a response payload out of the channel's buffer (it is only valid
/// for the duration of the callback). `None` = CB_RESPONSE_FAIL.
unsafe fn response_bytes(r: Option<(u16, *const BYTE, UINT32)>) -> Option<Vec<u8>> {
    let (flags, data, len) = r?;
    if flags & CB_RESPONSE_OK == 0 {
        return None;
    }
    if data.is_null() || len == 0 {
        return Some(Vec::new());
    }
    Some(std::slice::from_raw_parts(data, len as usize).to_vec())
}

// ── guest → host ────────────────────────────────────────────────────────

fn outbound_worker(s: Arc<Shared>, rx: Receiver<OutJob>) {
    let mut out = Outbound::default();
    while let Ok(job) = rx.recv() {
        match job {
            OutJob::Ready => {
                out.ready = true;
                if let Some(mimes) = out.pending.take() {
                    announce_local(&s, &mut out, mimes);
                }
            }
            OutJob::Local(mimes) => {
                if out.ready {
                    announce_local(&s, &mut out, mimes);
                } else {
                    out.pending = Some(mimes);
                }
            }
            OutJob::Data(id) => serve_data(&s, &mut out, id),
            OutJob::File {
                stream_id,
                list_index,
                flags,
                position,
                cb,
            } => serve_file(&s, &out, stream_id, list_index, flags, position, cb),
        }
    }
}

fn announce_local(s: &Shared, out: &mut Outbound, mimes: Vec<String>) {
    out.text_mime = fmt::best_text_mime(&mimes).map(str::to_string);
    out.files_mime = fmt::best_files_mime(&mimes);
    out.entries.clear();

    let mut ids: Vec<u32> = Vec::new();
    let mut names: Vec<Option<CString>> = Vec::new();
    if out.files_mime.is_some() {
        ids.push(fmt::SERVER_FORMAT_FILEGROUPDESCRIPTORW);
        names.push(CString::new(fmt::FILEGROUPDESCRIPTORW_NAME).ok());
        ids.push(fmt::SERVER_FORMAT_FILECONTENTS);
        names.push(CString::new(fmt::FILECONTENTS_NAME).ok());
    }
    if out.text_mime.is_some() {
        ids.push(fmt::CF_UNICODETEXT);
        names.push(None);
    }
    let name_ptrs: Vec<*const c_char> = names
        .iter()
        .map(|n| n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()))
        .collect();
    // An empty list is a valid announcement: the guest selection was cleared.
    s.with_ctx(|c| unsafe {
        wayrdp_cliprdr_send_format_list(c, ids.as_ptr(), name_ptrs.as_ptr(), ids.len() as u32)
    });
}

fn respond_data(s: &Shared, data: Option<&[u8]>) {
    s.with_ctx(|c| unsafe {
        match data {
            Some(d) => wayrdp_cliprdr_send_data_response(c, 1, d.as_ptr(), d.len() as u32),
            None => wayrdp_cliprdr_send_data_response(c, 0, std::ptr::null(), 0),
        }
    });
}

fn serve_data(s: &Shared, out: &mut Outbound, id: u32) {
    let Some(cb) = s.clipboard() else {
        return respond_data(s, None);
    };
    match id {
        fmt::CF_UNICODETEXT => {
            let Some(mime) = out.text_mime.as_deref() else {
                return respond_data(s, None);
            };
            match cb.read_local(mime, LOCAL_READ_TIMEOUT, MAX_TEXT_BYTES) {
                Ok(bytes) => {
                    let wire = fmt::text_to_utf16le(&String::from_utf8_lossy(&bytes));
                    respond_data(s, Some(&wire));
                }
                Err(e) => {
                    eprintln!("[cliprdr] reading local text failed: {e}");
                    respond_data(s, None);
                }
            }
        }
        fmt::SERVER_FORMAT_FILEGROUPDESCRIPTORW => {
            let Some(mime) = out.files_mime else {
                return respond_data(s, None);
            };
            let roots = match cb.read_local(mime, LOCAL_READ_TIMEOUT, MAX_TEXT_BYTES) {
                Ok(b) => fmt::parse_uri_list(&String::from_utf8_lossy(&b)),
                Err(e) => {
                    eprintln!("[cliprdr] reading local file list failed: {e}");
                    return respond_data(s, None);
                }
            };
            out.entries = fmt::collect_local_entries(&roots, MAX_LOCAL_ENTRIES);
            let descs: Vec<FileDescriptor> =
                out.entries.iter().map(|e| e.descriptor.clone()).collect();
            match fmt::encode_file_group_descriptor(&descs) {
                Ok(wire) => respond_data(s, Some(&wire)),
                Err(e) => {
                    eprintln!("[cliprdr] encoding file list failed: {e}");
                    respond_data(s, None);
                }
            }
        }
        _ => respond_data(s, None),
    }
}

fn serve_file(
    s: &Shared,
    out: &Outbound,
    stream_id: u32,
    list_index: u32,
    flags: u32,
    position: u64,
    cb: u32,
) {
    let result: Option<Vec<u8>> = (|| {
        let entry = out.entries.get(list_index as usize)?;
        if flags & FILECONTENTS_SIZE != 0 {
            let size = if entry.descriptor.is_dir {
                0
            } else {
                std::fs::metadata(&entry.path).ok()?.len()
            };
            return Some(size.to_le_bytes().to_vec());
        }
        if flags & FILECONTENTS_RANGE == 0 || entry.descriptor.is_dir {
            return None;
        }
        let mut f = std::fs::File::open(&entry.path).ok()?;
        f.seek(SeekFrom::Start(position)).ok()?;
        let mut buf = Vec::new();
        f.take(u64::from(cb.min(MAX_SERVE_CHUNK)))
            .read_to_end(&mut buf)
            .ok()?;
        Some(buf)
    })();
    s.with_ctx(|c| unsafe {
        match &result {
            Some(d) => wayrdp_cliprdr_send_file_contents_response(
                c,
                stream_id,
                1,
                d.as_ptr(),
                d.len() as u32,
            ),
            None => {
                wayrdp_cliprdr_send_file_contents_response(c, stream_id, 0, std::ptr::null(), 0)
            }
        }
    });
}

// ── host → guest ────────────────────────────────────────────────────────

fn event_loop(s: Arc<Shared>, rx: Receiver<ClipboardEvent>) {
    while let Ok(ev) = rx.recv() {
        match ev {
            ClipboardEvent::LocalSelection { mimes } => s.queue(OutJob::Local(mimes)),
            ClipboardEvent::SendRequest { mime, sink } => {
                let w = Arc::clone(&s);
                let _ = std::thread::Builder::new()
                    .name("wayrdp-clip-in".into())
                    .spawn(move || paste_remote(&w, &mime, sink));
            }
        }
    }
}

/// Request one format from the client and wait for its response.
fn request_data(s: &Shared, format_id: u32) -> Option<Vec<u8>> {
    let (tx, rx) = bounded(1);
    *s.data_waiter.lock().unwrap() = Some(tx);
    if !s.with_ctx(|c| unsafe { wayrdp_cliprdr_send_data_request(c, format_id) }) {
        s.data_waiter.lock().unwrap().take();
        return None;
    }
    let got = rx.recv_timeout(RESPONSE_TIMEOUT).ok().flatten();
    s.data_waiter.lock().unwrap().take();
    got
}

fn request_range(s: &Shared, list_index: u32, position: u64, cb: u32) -> Option<Vec<u8>> {
    let stream_id = s.next_stream.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = bounded(1);
    s.file_waiters.lock().unwrap().insert(stream_id, tx);
    let sent = s.with_ctx(|c| unsafe {
        wayrdp_cliprdr_send_file_contents_request(
            c,
            stream_id,
            list_index,
            FILECONTENTS_RANGE,
            position,
            cb,
        )
    });
    let got = if sent {
        rx.recv_timeout(RESPONSE_TIMEOUT).ok().flatten()
    } else {
        None
    };
    s.file_waiters.lock().unwrap().remove(&stream_id);
    got
}

fn paste_remote(s: &Shared, mime: &str, mut sink: Box<dyn Write + Send>) {
    let _one_at_a_time = s.fetch_lock.lock().unwrap();
    let (seq, text_format, files_format, cached) = {
        let r = s.remote.lock().unwrap();
        let cached = r
            .files_cache
            .as_ref()
            .filter(|(sq, _)| *sq == r.seq)
            .map(|(_, p)| p.clone());
        (r.seq, r.text_format, r.files_format, cached)
    };

    if fmt::is_text_mime(mime) {
        let Some(id) = text_format else { return };
        let Some(data) = request_data(s, id) else {
            eprintln!("[cliprdr] client did not deliver text");
            return;
        };
        let text = if id == fmt::CF_UNICODETEXT {
            fmt::utf16le_to_text(&data)
        } else {
            fmt::ansi_to_text(&data)
        };
        let _ = sink.write_all(text.as_bytes());
        return;
    }

    if mime == fmt::URI_LIST_MIME || mime == fmt::GNOME_COPIED_FILES_MIME {
        let Some(id) = files_format else { return };
        let paths = match cached {
            Some(p) => p,
            None => match download_files(s, id, seq) {
                Some(p) => {
                    let mut r = s.remote.lock().unwrap();
                    if r.seq == seq {
                        r.files_cache = Some((seq, p.clone()));
                    }
                    p
                }
                None => return,
            },
        };
        let payload = if mime == fmt::URI_LIST_MIME {
            fmt::build_uri_list(&paths)
        } else {
            fmt::build_gnome_copied_files(&paths)
        };
        let _ = sink.write_all(payload.as_bytes());
    }
}

fn staging_root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("wayrdp").join("clipboard"))
}

/// Reap staging trees older than `SweepAge` at channel start. Teardown
/// deliberately leaves staging in place (a disconnect mid-paste must not
/// delete files a local app is still reading); this sweep is what keeps
/// abandoned trees from accumulating forever. Fresh trees — including any
/// just created by a concurrent peer of the same user — are never touched.
const STAGING_SWEEP_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

fn sweep_stale_staging() {
    if let Some(root) = staging_root() {
        sweep_dir(&root);
    }
}

fn sweep_dir(root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(md) = entry.metadata() else { continue };
        if !md.is_dir() {
            continue;
        }
        let stale = md
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > STAGING_SWEEP_AGE);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Fetch the client's file list and download every file into a fresh
/// staging directory. Returns the top-level paths to hand to the paster.
fn download_files(s: &Shared, files_format: u32, seq: u64) -> Option<Vec<PathBuf>> {
    let wire = request_data(s, files_format)?;
    let descs = match fmt::decode_file_group_descriptor(&wire) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[cliprdr] bad FileGroupDescriptorW from client: {e}");
            return None;
        }
    };
    if descs.len() > MAX_REMOTE_ENTRIES {
        eprintln!(
            "[cliprdr] announcement has {} entries (max {MAX_REMOTE_ENTRIES}); refusing download",
            descs.len()
        );
        return None;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = staging_root()?.join(format!("{stamp}-{seq}"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[cliprdr] cannot create staging dir {}: {e}", dir.display());
        return None;
    }
    // Superseded staging trees from earlier announcements of this peer are
    // dead weight (files_cache only serves the newest seq); remove them so
    // one long-lived connection cannot accumulate one budget's worth per
    // paste. NOTE: a tree the local user may still be reading (a slow copy
    // out of the staging path) is NOT deleted on disconnect — teardown
    // leaves staging alone, and a startup sweep reaps only old trees (see
    // sweep_stale_staging).
    {
        let mut staging = s.staging.lock().unwrap();
        for old in staging.drain(..) {
            if old != dir {
                let _ = std::fs::remove_dir_all(old);
            }
        }
        staging.push(dir.clone());
    }

    // The client controls every declared size AND every response body, so
    // the budget is decremented by bytes actually written. An entry that
    // cannot fit the budget fails the WHOLE paste visibly (an empty paste
    // plus this log) rather than silently delivering a partial tree; an
    // operator who legitimately transfers more raises the budget with
    // WAYRDP_DOWNLOAD_BUDGET_MB.
    let mut remaining: u64 = download_budget();

    let mut tops: Vec<PathBuf> = Vec::new();
    for (index, d) in descs.iter().enumerate() {
        let Some(rel) = fmt::sanitize_relative(&d.name) else {
            eprintln!(
                "[cliprdr] refusing unsafe file name from client: {:?}",
                d.name
            );
            continue;
        };
        let target = dir.join(&rel);
        if d.is_dir {
            if std::fs::create_dir_all(&target).is_err() {
                return None;
            }
            if rel.components().count() == 1 {
                tops.push(target.clone());
            }
            continue;
        }
        if d.size > remaining {
            eprintln!(
                "[cliprdr] entry {:?} declares {} bytes, over the download budget ({} left of {}); failing the paste (raise with WAYRDP_DOWNLOAD_BUDGET_MB)",
                d.name,
                d.size,
                remaining,
                download_budget()
            );
            return None;
        }
        if let Some(parent) = target.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return None;
            }
        }
        if !download_one(s, index as u32, d.size, &target, &mut remaining) {
            eprintln!("[cliprdr] download of {:?} failed", d.name);
            let _ = std::fs::remove_file(&target);
            return None;
        }
        if rel.components().count() == 1 {
            tops.push(target.clone());
        }
    }
    Some(tops)
}

fn download_one(
    s: &Shared,
    list_index: u32,
    size: u64,
    target: &PathBuf,
    remaining: &mut u64,
) -> bool {
    let Ok(mut f) = std::fs::File::create(target) else {
        return false;
    };
    let mut pos = 0u64;
    while pos < size {
        let want = (size - pos).min(u64::from(RANGE_CHUNK)).min(*remaining) as u32;
        if want == 0 {
            // Budget exhausted mid-file: abort the announcement rather
            // than truncate silently.
            return false;
        }
        let Some(chunk) = request_range(s, list_index, pos, want) else {
            return false;
        };
        if chunk.is_empty() {
            return false;
        }
        let take = accepted_chunk_len(chunk.len(), want, *remaining);
        if f.write_all(&chunk[..take]).is_err() {
            return false;
        }
        *remaining -= take as u64;
        pos += take as u64;
    }
    true
}

/// Bytes of a client FILECONTENTS response to accept for a request of
/// `want` bytes: the response length is client-controlled wire data, so it
/// may never exceed what was requested of it, and never the budget.
fn accepted_chunk_len(chunk_len: usize, want: u32, remaining: u64) -> usize {
    chunk_len
        .min(want as usize)
        .min(remaining.min(u32::MAX as u64) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_chunks_are_clamped_to_the_request() {
        // A client answering a 1 MiB range request with more bytes than
        // were asked for must not inflate the staged file past the
        // announced size: only `want` bytes are accepted.
        assert_eq!(
            accepted_chunk_len(2 * 1024 * 1024, RANGE_CHUNK, u64::MAX),
            RANGE_CHUNK as usize
        );
        assert_eq!(accepted_chunk_len(4096, 4096, u64::MAX), 4096);
        assert_eq!(accepted_chunk_len(100, 4096, u64::MAX), 100);
        // Budget always binds, even against an oversized chunk.
        assert_eq!(accepted_chunk_len(4096, RANGE_CHUNK, 1000), 1000);
        assert_eq!(accepted_chunk_len(4096, 4096, 0), 0);
    }

    #[test]
    fn download_caps_are_sane() {
        // The announcement gate mirrors the serving-side entry cap, and the
        // aggregate budget bounds every response body a peer can land on
        // disk for one announcement.
        assert_eq!(MAX_REMOTE_ENTRIES, MAX_LOCAL_ENTRIES);
        assert!(download_budget() >= MAX_TEXT_BYTES as u64);
    }

    #[test]
    fn sweep_reaps_only_old_staging_trees() {
        let root =
            std::env::temp_dir().join(format!("wayrdp-cliprdr-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let fresh = root.join("fresh-1-1");
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(fresh.join("f.txt"), b"x").unwrap();
        let old = root.join("old-1-1");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("f.txt"), b"x").unwrap();
        // Backdate the old tree past the sweep age.
        let past =
            std::time::SystemTime::now() - STAGING_SWEEP_AGE - std::time::Duration::from_secs(60);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();

        // Sweep this root directly (sweep_stale_staging would use the
        // process's real cache dir).
        sweep_dir(&root);

        assert!(fresh.exists(), "fresh tree must survive the sweep");
        assert!(!old.exists(), "aged tree must be reaped");
        let _ = std::fs::remove_dir_all(&root);
    }
}
