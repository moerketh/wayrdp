//! Per-connection peer state and the D3 protocol callbacks.
//!
//! One [`PeerContext`] lives per accepted client. Its memory layout matches
//! what FreeRDP expects: the base `rdpContext` is the first field, so
//! `client->context` (set by `freerdp_peer_context_new`) points at our struct.

use std::ffi::CString;
use std::ptr;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::Instant;

use freerdp_sys::*;

use crate::consts::*;
use crate::pointer;
use desk_backend::{
    CaptureSession, CaptureTarget, Frame, InputEvent, PointerButton, SessionRequest, Size,
    StampedInput,
};

/// Per-connection context. `base` must remain the first field so FreeRDP's
/// `client->context` cast and our `as *mut PeerContext` agree.
#[repr(C)]
pub struct PeerContext {
    /// FreeRDP-allocated base context; its back-links are set by
    /// `freerdp_peer_context_new`.
    pub base: rdp_context,
    pub peer: *mut rdp_freerdp_peer,
    /// The connected transport socket, captured in peer_accepted before
    /// freerdp_peer_context_new hands it to the transport and sets
    /// client->sockfd = -1 (freerdp_peer_transport_setup, libfreerdp
    /// peer.c). -1 = "no EOF check" (also the calloc default is 0 = stdin,
    /// so it is set explicitly). FreeRDP's transport OWNS the fd: wayrdp
    /// only ever recv(MSG_PEEK)s it from the mainloop, never reads into,
    /// writes to, or closes it, so a reused fd number can't be hit — the
    /// check only runs while the transport is alive.
    pub peer_fd: i32,
    /// VCM server handle; owns drdynvc so mstsc finishes channel setup.
    /// Mandatory since D4: EGFX, disp, etc. are layered on top of it via
    /// FreeRDP's built-in server-side WTS API table (registered in
    /// Server::run_freerdp).
    pub vcm: HANDLE,
    /// EGFX (Graphics Pipeline) server channel; null unless the client
    /// negotiated SupportGraphicsPipeline (post_connect creates it then).
    pub rdpgfx: *mut RdpgfxServerContext,
    /// Display-Control (RDPEDISP) server channel; null unless the client
    /// advertised disp support (D5). Owns its reader thread after Open.
    pub disp: *mut s_disp_server_context,
    /// True once the deferred disp Open succeeded (drdynvc READY).
    pub disp_opened: bool,
    /// Resize requested by the client via Display-Control monitor layout
    /// (written from the disp reader thread, consumed by the mainloop).
    /// Mutex-guarded (F6): the writer and consumer run on different threads.
    /// calloc can't construct this — ptr::write in peer_context_new,
    /// drop_in_place in peer_context_free.
    pub disp_req: std::sync::Mutex<Option<Size>>,
    /// Clipboard redirection (cliprdr static channel); None unless the
    /// client joined the channel and the backend offers a clipboard.
    /// calloc-zero is a valid None; dropped before the VCM closes.
    pub cliprdr: Option<Box<crate::cliprdr::Cliprdr>>,
    /// RemoteFX encoder for the current gfx surface; recreated with it.
    /// Owned via the freerdp-sys shim (RFX_CONTEXT + one reusable wStream).
    pub rfx: *mut WayrdpRfx,
    /// True once the deferred Open succeeded (drdynvc READY). Before that,
    /// rdpgfx is created+initialized but has no channel/event handle.
    pub gfx_opened: bool,
    /// True once CapsAdvertise was confirmed and the gfx surface is
    /// created+mapped; the frame pump then takes the EGFX path.
    pub gfx_ready: bool,
    /// True once a gfx surface id is live on the client; DeleteSurface is
    /// only legal then (mstsc parses DeleteSurface of an unknown id as a
    /// hard protocol error and resets).
    pub gfx_surface_exists: bool,
    /// EGFX back-pressure: surface frames handed to the channel. Paired
    /// with gfx_frames_acked as our own send/ack bookkeeping — the client
    /// counters inside FrameAcknowledge (queueDepth, totalFramesDecoded)
    /// reset at moments we cannot observe (on CapsConfirm, not on
    /// ResetGraphics), so gating on them wraps the arithmetic and stalls
    /// the pipeline for good.
    pub gfx_frames_sent: UINT32,
    /// FrameAcknowledge PDUs received (one per decoded frame).
    pub gfx_frames_acked: UINT32,
    /// Client stopped acknowledging frames intentionally (queueDepth =
    /// SUSPEND_FRAME_ACKNOWLEDGEMENT, MS-RDPEGFX 2.2.3.10). When set, the
    /// in-flight window no longer gates sends; cleared on CapsConfirm.
    pub gfx_ack_suspended: bool,
    // PeerContext is calloc'd by FreeRDP (freerdp_peer_context_new with
    // ContextSize), not a Rust Box. All-zero bytes are a valid
    // Option<Instant> (Some of time zero) and Instant has no Drop, so
    // plain assignment over the calloc'd zeroes is fine;
    // peer_context_new_inner stores None right away to make it
    // unambiguous. Fat non-POD types would still be UB here.
    /// Wall clock of when the in-flight window last became full (from the
    /// first send if no ack has ever arrived), so a client with acks
    /// disabled still trips the safety net. Cleared by the ack handler
    /// once the backlog drains.
    pub gfx_window_full_since: Option<Instant>,
    /// Rate-limiter for corrective backend resizes when its frame size drifts
    /// from the negotiated surface size (one resize attempt per N mismatched
    /// frames, so a slow-transitioning backend doesn't trigger a storm).
    pub gfx_resize_correct_ctr: UINT32,
    /// Next surface-frame id (shared between slow-path markers and EGFX).
    pub frame_id: UINT32,
    /// True once `Activate` has run.
    pub activated: bool,
    /// Transparent-pointer resend flag: set by activate/failed first-send,
    /// consumed once the slow-path frame loop (send_frame) re-sends it.
    pub pointer_resend: bool,
    /// In-flight frame id awaiting client acknowledge (frame-marker
    /// back-pressure; None when markers unsupported or no frame outstanding).
    pub pending_ack: Option<UINT32>,
    /// Frame source: the synthetic backend pushes frames here at ~30 fps.
    pub frame_rx: Option<Receiver<Frame>>,
    /// e6: WinPR manual-reset event signalled by the backend after each
    /// frame send. Joined into the mainloop wait set so a published frame
    /// wakes the loop immediately instead of waiting out the poll timeout.
    /// NULL until peer_post_connect creates it.
    pub frame_event: HANDLE,
    /// Direct `Arc`-shared latest-position cell; the backend writes it on
    /// every injected motion, the mainloop polls it per frame and relays
    /// newest-wins as `PTR_POS_UPDATE` (Metadata mode). Only Some when
    /// WAYRDP_POINTER_METADATA is set — see peer_post_connect for why this
    /// is off by default.
    pub pointer_pos_slot: Option<desk_backend::PointerPositionSlot>,
    /// Last raw value read from `pointer_pos_slot`; used to only emit
    /// PTR_POS_UPDATE on change.
    pub pointer_pos_last: u64,
    /// Client-drawn cursor (WAYRDP_CLIENT_CURSOR): the backend publishes the
    /// compositor's cursor image here and leaves it out of the frames; the
    /// mainloop sends it as the client's pointer shape. None = the cursor is
    /// painted in the video behind a transparent client pointer.
    pub cursor_slot: Option<desk_backend::PointerShapeSlot>,
    /// Last cursor state taken from `cursor_slot`, re-sent whenever the
    /// client's pointer state resets (activation, EGFX reset).
    pub cursor_last: Option<desk_backend::CursorUpdate>,
    /// Live capture session; kept for resize/inject once D4/D5 need them.
    pub session: Option<Box<dyn CaptureSession>>,
    pub width: UINT32,
    pub height: UINT32,
    /// Frames that fell back to a full-surface rect because damage was
    /// empty. If this tracks `frames_sent`, the compositor isn't emitting
    /// granular damage (Hyprland's ext-image-copy may report full surface).
    pub damage_full_frames: u64,
    /// Previous frame's pixels for the WAYRDP_TILED_DIFF damage-detection
    /// path. Box-allocated to keep PeerContext cheap to calloc; None until
    /// the first send_frame_gfx allocates it.
    pub prev_frame_pixels: Option<Box<[u8]>>,
}

/// Global listener state, boxed and passed via `listener->info` so
/// `peer_accepted` can reach it on any OS thread.
pub struct ServerState {
    pub cert_path: CString,
    pub key_path: CString,
    pub open_session: Box<dyn Fn(SessionRequest) -> Option<Box<dyn CaptureSession>> + Send + Sync>,
    /// The backend's session clipboard, opened per peer (None = no cliprdr).
    pub open_clipboard: Box<dyn Fn() -> Option<Box<dyn desk_backend::Clipboard>> + Send + Sync>,
    /// Allowed peer CIDs for vsock connections (empty = any). Only checked
    /// for AF_VSOCK peers; TCP peers bypass it entirely.
    pub vsock_allow: Vec<u32>,
    /// winpr SAM (NTLM verifier) file for NLA on TCP peers. `None` on
    /// vsock-only deployments; Server::run refuses TCP without it.
    pub sam_path: Option<CString>,
}

// Raw pointers are not Send; PeerThreadArgs packages the two pointers the
// per-peer thread needs. FreeRDP guarantees both outlive the connection, and
// the spawned thread is the only owner after peer_accepted returns.
struct PeerThreadArgs(*mut rdp_freerdp_peer, *mut PeerContext);
unsafe impl Send for PeerThreadArgs {}

/// Every extern callback below runs on a FreeRDP thread; a panic unwinding
/// across the FFI boundary is UB, so all of them route through this guard.
/// `None` on panic: the caller converts that to the callback's failure
/// return (0 / FALSE), which makes FreeRDP abandon the peer connection.
fn ffi_guard<T>(f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(_) => {
            // eprintln! PANICS when stderr is broken (observed live: a full
            // per-user /tmp quota made every stderr write fail; the first
            // inner eprintln! panicked, was caught here, and THIS handler's
            // own eprintln! then panicked across the extern "C" boundary ->
            // panic_cannot_unwind -> the daemon abort-looped on every
            // connection). Always write with an error-ignoring call so a
            // trapped panic degrades to a rejected session, never a daemon
            // kill.
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[wayrdp] panic in FreeRDP callback; rejecting the peer"
            );
            None
        }
    }
}

/// Clamp a client-proposed desktop size to what the backend and the EGFX
/// wire format can carry (MS-RDPEDISP allows 200..=8192 per dimension;
/// UINT16 surface fields bound us at 65535). Both dimensions snap down to
/// even: the backends even out the output size on their side too, and an
/// asymmetrical clamp (even width, odd height) makes the backend's frame
/// size diverge from the negotiated size forever — the corrective-resize
/// loop / black-screen symptom (F2).
pub fn clamp_size(w: UINT32, h: UINT32) -> (UINT32, UINT32) {
    let w = w.clamp(200, 8192) & !1;
    let h = h.clamp(200, 8192) & !1;
    (w, h)
}

pub unsafe extern "C" fn peer_accepted(
    instance: *mut rdp_freerdp_listener,
    client: *mut rdp_freerdp_peer,
) -> BOOL {
    match ffi_guard(|| peer_accepted_inner(instance, client)) {
        Some(rc) => rc,
        None => 0,
    }
}

unsafe fn peer_accepted_inner(
    instance: *mut rdp_freerdp_listener,
    client: *mut rdp_freerdp_peer,
) -> BOOL {
    eprintln!(
        "[peer_accepted] enter instance={:?} client={:?}",
        instance, client
    );
    if instance.is_null() || client.is_null() {
        eprintln!("[peer_accepted] null arg, bailing");
        return 0;
    }
    let state = (*instance).info as *mut ServerState;
    if state.is_null() {
        eprintln!("[peer_accepted] listener->info null, bailing");
        return 0;
    }
    let state = &mut *state;
    eprintln!("[peer_accepted] state ok, cert={:?}", state.cert_path);

    // vsock allowlist: refuse connections from non-listed CIDs before any
    // protocol work. TCP peers (getpeername yields AF_INET) skip the check.
    // Remember the socket family: the security-layer config below is
    // per-family, so the same listener can serve vmconnect (vsock) and
    // mstsc/FreeRDP (TCP).
    let peer_cid = crate::vsock::peer_cid((*client).sockfd);
    if let Some(cid) = peer_cid {
        if !state.vsock_allow.is_empty() && !state.vsock_allow.contains(&cid) {
            eprintln!(
                "[peer_accepted] vsock peer cid={} not in allowlist; rejecting",
                cid
            );
            return 0;
        }
        eprintln!("[peer_accepted] vsock peer cid={} accepted", cid);
    }
    let is_vsock = peer_cid.is_some();

    // Capture the connected socket before freerdp_peer_context_new runs:
    // freerdp_peer_transport_setup hands it to the transport and then sets
    // client->sockfd = -1, so any check must keep its own copy.
    let peer_fd = (*client).sockfd;

    (*client).ContextSize = std::mem::size_of::<PeerContext>();
    (*client).ContextNew = Some(peer_context_new);
    (*client).ContextFree = Some(peer_context_free);
    if freerdp_peer_context_new(client) == 0 {
        eprintln!("[peer_accepted] freerdp_peer_context_new failed");
        return 0;
    }
    eprintln!(
        "[peer_accepted] context_new ok, context={:?}",
        (*client).context
    );

    // Pass ServerState through ContextExtra so PostConnect can reach it.
    // freerdp_peer_context_new already zeroed ContextExtra; we fill it now.
    (*client).ContextExtra = state as *mut ServerState as *mut _;

    let ctx = (*client).context as *mut PeerContext;
    (*ctx).peer = client;
    (*ctx).peer_fd = peer_fd;
    // FreeRDP's built-in server WTS table is registered once at listener
    // startup (Server::run_freerdp), so WTSOpenServerA peers a real
    // in-process VCM keyed on the rdpContext pointer. D4's EGFX channel is
    // layered on top of it, so failure here is fatal (was optional in D3
    // when slow-path SurfaceBits was the only output).
    (*ctx).vcm = wayrdp_wts_open_server(ctx as *mut _);
    if (*ctx).vcm.is_null() {
        eprintln!("[peer_accepted] WTSOpenServer failed; rejecting peer");
        return 0;
    }
    // Do NOT call WTSVirtualChannelManagerOpen here: it flips drdynvc to
    // INITIALIZED *before* opening the channel, and at accept time no client
    // has joined drdynvc yet, so the capabilities request would be silently
    // dropped and the state machine stuck. The mainloop opens it once
    // IsChannelJoined reports TRUE.
    eprintln!("[peer_accepted] vcm ok, vcm={:?}", (*ctx).vcm);

    let cert = freerdp_certificate_new_from_file(state.cert_path.as_ptr());
    let key = freerdp_key_new_from_file_enc(state.key_path.as_ptr(), ptr::null());
    if cert.is_null() || key.is_null() {
        if !cert.is_null() {
            freerdp_certificate_free(cert);
        }
        if !key.is_null() {
            freerdp_key_free(key);
        }
        return 0;
    }
    let settings = (*(*client).context).settings;
    freerdp_settings_set_pointer_len(
        settings,
        FreeRDP_Settings_Keys_Pointer_FreeRDP_RdpServerCertificate,
        cert as *const _,
        1,
    );
    freerdp_settings_set_pointer_len(
        settings,
        FreeRDP_Settings_Keys_Pointer_FreeRDP_RdpServerRsaKey,
        key as *const _,
        1,
    );
    // Per-family security-layer config (the transport story is in the
    // branches below).
    freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_TlsSecurity, 1);
    freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_ExtSecurity, 0);
    if is_vsock {
        // vmconnect Enhanced Session reaches us over AF_VSOCK and negotiates
        // the legacy RDP ("standard") security layer — no TLS, no X.224 nego
        // fallback (observed: tlsany_meth "wrong version number" as the
        // client sends a raw MCS connect-initial where the TLS server
        // expected a ClientHello). Advertising RdpSecurity accepts that
        // proposal; FreeRDP skips the client random/RC4 key establishment
        // because EncryptionMethods advertises ENCRYPTION_METHOD_NONE, and
        // ENCRYPTION_LEVEL_NONE skips data encryption, so no RC4/MD4 is
        // required on the wire. FreeRDP must NOT run its own TLS accept on
        // the vsock stream: VmConnectMode short-circuits transport_accept_tls
        // in rdp_server_accept_nego.
        freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_RdpSecurity, 1);
        freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_NlaSecurity, 0);
        freerdp_settings_set_bool(
            settings,
            FreeRDP_Settings_Keys_Bool_FreeRDP_VmConnectMode,
            1,
        );
        freerdp_settings_set_uint32(
            settings,
            wayrdp_settings_key_uint32_encryption_methods() as FreeRDP_Settings_Keys_UInt32,
            wayrdp_encryption_method_none(),
        );
        freerdp_settings_set_uint32(
            settings,
            FreeRDP_Settings_Keys_UInt32_FreeRDP_EncryptionLevel,
            wayrdp_encryption_level_none(),
        );
    } else {
        // Plain TCP (mstsc / xfreerdp / debug tools): NLA only. CredSSP over
        // TLS authenticates against the provisioned winpr SAM file — the
        // only transport here is one the operator explicitly opted into, so
        // there is no host proxy to lean on and no fallback to
        // unauthenticated security layers. The binary refuses to serve TCP
        // without a SAM path (see server main), so a missing path here is a
        // bug, not a configuration state to tolerate.
        freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_RdpSecurity, 0);
        freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_NlaSecurity, 1);
        // TLS must stay OFF as a standalone layer here: the shared setup
        // above enables TlsSecurity, and with both TLS and NLA offered,
        // FreeRDP's negotiation accepts a client that proposes TLS alone
        // (xfreerdp /sec:tls) and CredSSP never runs. NLA-only means the
        // client must offer HYBRID; TLS-only proposals are rejected with
        // HYBRID_REQUIRED.
        freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_TlsSecurity, 0);
        freerdp_settings_set_bool(
            settings,
            FreeRDP_Settings_Keys_Bool_FreeRDP_VmConnectMode,
            0,
        );
        let Some(sam) = state.sam_path.as_ref() else {
            eprintln!("[peer_accepted] TCP peer but no NLA SAM configured; rejecting");
            return 0;
        };
        if freerdp_settings_set_string(
            settings,
            FreeRDP_Settings_Keys_String_FreeRDP_NtlmSamFile,
            sam.as_ptr(),
        ) == 0
        {
            eprintln!("[peer_accepted] setting NtlmSamFile failed; rejecting");
            return 0;
        }
    }
    freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_RefreshRect, 1);
    freerdp_settings_set_bool(
        settings,
        FreeRDP_Settings_Keys_Bool_FreeRDP_SuppressOutput,
        1,
    );
    // transport_default_write only pushes the buffered socket BIO past its write
    // buffer when `transport->blocking || WaitForOutputBufferFlush`. wayrdp's vsock
    // (and non-blocking TCP) accept leaves the transport non-blocking and never set
    // this flag, so a lone small write (e.g. the 4-byte drdynvc capability request)
    // sat in BIO_s_buffered_socket's buffer and never reached the wire — observed as
    // "SendChannelData rc=1, host receives nothing". Setting it makes every write
    // flush through to the fd immediately.
    freerdp_settings_set_bool(
        settings,
        FreeRDP_Settings_Keys_Bool_FreeRDP_WaitForOutputBufferFlush,
        1,
    );
    freerdp_settings_set_bool(
        settings,
        FreeRDP_Settings_Keys_Bool_FreeRDP_SurfaceFrameMarkerEnabled,
        1,
    );
    freerdp_settings_set_bool(
        settings,
        FreeRDP_Settings_Keys_Bool_FreeRDP_RemoteFxCodec,
        1,
    );
    freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_NSCodec, 1);
    freerdp_settings_set_uint32(
        settings,
        FreeRDP_Settings_Keys_UInt32_FreeRDP_ColorDepth,
        32,
    );
    freerdp_settings_set_uint32(
        settings,
        FreeRDP_Settings_Keys_UInt32_FreeRDP_MultifragMaxRequestSize,
        0xFFFFFF,
    );
    // Input capability advertisement (server TS_INPUT_CAPABILITYSET is
    // assembled from these): the zeroed server-mode settings defaults leave
    // UnicodeInput/FastPathInput off, and strictly compliant clients
    // (mstsc/vmconnect — unlike xfreerdp, which always sends) then believe
    // the server only accepts scancode-only input. gnome-remote-desktop,
    // which serves mstsc keyboards fine over the same FreeRDP server API,
    // sets exactly these explicitly.
    freerdp_settings_set_bool(
        settings,
        FreeRDP_Settings_Keys_Bool_FreeRDP_FastPathInput,
        1,
    );
    freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_UnicodeInput, 1);
    // NB: HasRelativeMouseEvent deliberately stays off — advertising it
    // without delivering relative motion (peer_input_rel_mouse discards
    // events) breaks pen/RTS clients that would otherwise use absolute
    // coordinates. Enable together with the implementation.
    freerdp_settings_set_bool(
        settings,
        FreeRDP_Settings_Keys_Bool_FreeRDP_HasHorizontalWheel,
        1,
    );
    // D4/EGFX: offer the Graphics Pipeline dynamic channel. H.264 (AVC420/
    // AVC444) stays off — the distro FreeRDP build has no OpenH264 encoder
    // and x264 is license-locked out of the process (locked decision); the
    // Phase 0 gfx codec is RemoteFX.
    freerdp_settings_set_bool(
        settings,
        FreeRDP_Settings_Keys_Bool_FreeRDP_SupportGraphicsPipeline,
        1,
    );
    freerdp_settings_set_bool(settings, FreeRDP_Settings_Keys_Bool_FreeRDP_GfxH264, 0);
    freerdp_settings_set_uint32(
        settings,
        FreeRDP_Settings_Keys_UInt32_FreeRDP_DesktopWidth,
        1280,
    );
    freerdp_settings_set_uint32(
        settings,
        FreeRDP_Settings_Keys_UInt32_FreeRDP_DesktopHeight,
        800,
    );

    (*client).PostConnect = Some(peer_post_connect);
    (*client).Activate = Some(peer_activate);
    if !(*(*client).context).input.is_null() {
        (*(*(*client).context).input).SynchronizeEvent = Some(peer_input_sync);
        (*(*(*client).context).input).KeyboardEvent = Some(peer_input_keyboard);
        (*(*(*client).context).input).UnicodeKeyboardEvent = Some(peer_input_unicode);
        (*(*(*client).context).input).MouseEvent = Some(peer_input_mouse);
        (*(*(*client).context).input).RelMouseEvent = Some(peer_input_rel_mouse);
        (*(*(*client).context).input).ExtendedMouseEvent = Some(peer_input_extended_mouse);
    }

    let ok = ((*client).Initialize.expect("client.Initialize"))(client);
    if ok == 0 {
        eprintln!("[peer_accepted] client.Initialize returned 0");
        return 0;
    }
    eprintln!("[peer_accepted] Initialize ok; spawning mainloop thread");

    let args = PeerThreadArgs(client, ctx);
    eprintln!("[peer_accepted] PeerThreadArgs ok, spawning");
    let (client_addr, ctx_addr) = (args.0 as usize, args.1 as usize);
    std::thread::spawn(move || unsafe {
        peer_mainloop(
            client_addr as *mut rdp_freerdp_peer,
            ctx_addr as *mut PeerContext,
        );
    });
    1
}

// freerdp_peer_context_new allocates PeerContext with calloc, so non-trivial
// fields (Option<Box<dyn CaptureSession>>, Option<Receiver<Frame>>) hold raw
// zero bytes, not valid Rust values. Zero is a valid bit pattern for these
// (both represent None), so we initialize them with ptr::write to avoid
// running a Drop on garbage. Thin plain-data/scalar fields use normal
// assignment.
unsafe extern "C" fn peer_context_new(_peer: *mut rdp_freerdp_peer, ctx: *mut rdpContext) -> BOOL {
    match ffi_guard(|| peer_context_new_inner(ctx)) {
        Some(rc) => rc,
        None => 0,
    }
}

unsafe fn peer_context_new_inner(ctx: *mut rdpContext) -> BOOL {
    eprintln!("[peer_context_new] context={:?}", ctx);
    let ctx = ctx as *mut PeerContext;
    // Construct the trait-object/receiver Options without dropping the
    // calloc'd bits.
    ptr::write(&mut (*ctx).session, None);
    ptr::write(&mut (*ctx).frame_rx, None);
    ptr::write(&mut (*ctx).pointer_pos_slot, None);
    (*ctx).pointer_pos_last = 0;
    ptr::write(&mut (*ctx).cursor_slot, None);
    ptr::write(&mut (*ctx).cursor_last, None);
    (*ctx).damage_full_frames = 0;
    ptr::write(&mut (*ctx).prev_frame_pixels, None);
    ptr::write(&mut (*ctx).pending_ack, None);
    ptr::write(&mut (*ctx).disp_req, std::sync::Mutex::new(None));
    ptr::write(&mut (*ctx).cliprdr, None);
    // Plain scalars / thin handles.
    (*ctx).peer = ptr::null_mut();
    (*ctx).vcm = ptr::null_mut();
    (*ctx).rdpgfx = ptr::null_mut();
    (*ctx).rfx = ptr::null_mut();
    (*ctx).frame_event = ptr::null_mut();
    (*ctx).gfx_opened = false;
    (*ctx).gfx_ready = false;
    (*ctx).gfx_surface_exists = false;
    (*ctx).gfx_frames_sent = 0;
    (*ctx).gfx_frames_acked = 0;
    (*ctx).gfx_ack_suspended = false;
    (*ctx).gfx_window_full_since = None;
    (*ctx).gfx_resize_correct_ctr = 0;
    (*ctx).frame_id = 1;
    (*ctx).activated = false;
    (*ctx).pointer_resend = false;
    (*ctx).width = 0;
    (*ctx).height = 0;
    (*ctx).disp = ptr::null_mut();
    (*ctx).disp_opened = false;
    eprintln!("[peer_context_new] initialized ok");
    1
}

unsafe extern "C" fn peer_context_free(_peer: *mut rdp_freerdp_peer, ctx: *mut rdpContext) {
    let _ = ffi_guard(|| peer_context_free_inner(ctx));
}

unsafe fn peer_context_free_inner(ctx: *mut rdpContext) {
    if ctx.is_null() {
        return;
    }
    let ctx = ctx as *mut PeerContext;
    // Teardown timeline: context teardown begins. rdpgfx borrows the VCM
    // handle; free it (and the RFX encoder) first.
    eprintln!("[peer_context_free_inner] teardown: context_free start");
    // cliprdr: Stop joins its channel thread; must precede the VCM close.
    (*ctx).cliprdr = None;
    if !(*ctx).rdpgfx.is_null() {
        wayrdp_rdpgfx_call_close((*ctx).rdpgfx);
        rdpgfx_server_context_free((*ctx).rdpgfx);
        (*ctx).rdpgfx = ptr::null_mut();
    }
    // disp owns its reader thread; Close joins it. Must happen before the
    // VCM handle below goes away.
    if !(*ctx).disp.is_null() {
        disp_server_context_free((*ctx).disp);
        (*ctx).disp = ptr::null_mut();
    }
    if !(*ctx).rfx.is_null() {
        wayrdp_rfx_free((*ctx).rfx);
        (*ctx).rfx = ptr::null_mut();
    }
    if !(*ctx).vcm.is_null() {
        wayrdp_wts_close_server((*ctx).vcm);
        (*ctx).vcm = ptr::null_mut();
    }
    (*ctx).frame_rx = None;
    (*ctx).pointer_pos_slot = None;
    (*ctx).cursor_slot = None;
    (*ctx).cursor_last = None;
    // Dropping the capture session closes its cmd channel -> pump loop notices
    // and tears down its headless output; the pump's timeline continues in
    // backend-wlroots under "teardown wayrdp-N:".
    (*ctx).session = None;
    eprintln!("[peer_context_free_inner] teardown: session dropped");
    (*ctx).prev_frame_pixels = None;
    if !(*ctx).frame_event.is_null() {
        wayrdp_close_handle((*ctx).frame_event);
        (*ctx).frame_event = ptr::null_mut();
    }
    // Dropping the Mutex frees its allocation; only session/frame_rx/
    // pending_ack are also non-trivial and were reset normally above.
    unsafe { ptr::drop_in_place(&mut (*ctx).disp_req) };
}

unsafe extern "C" fn peer_post_connect(client: *mut rdp_freerdp_peer) -> BOOL {
    match ffi_guard(|| peer_post_connect_inner(client)) {
        Some(rc) => rc,
        None => 0,
    }
}

unsafe fn peer_post_connect_inner(client: *mut rdp_freerdp_peer) -> BOOL {
    eprintln!("[peer_post_connect] enter, client={:?}", client);
    let ctx = (*client).context as *mut PeerContext;
    // Clamp the client-proposed geometry first (S2): DesktopWidth/Height in
    // settings come straight off the wire and feed the backend session as
    // well as UINT16 wire fields.
    ((*ctx).width, (*ctx).height) = clamp_size(
        freerdp_settings_get_uint32(
            (*(*client).context).settings,
            FreeRDP_Settings_Keys_UInt32_FreeRDP_DesktopWidth,
        ),
        freerdp_settings_get_uint32(
            (*(*client).context).settings,
            FreeRDP_Settings_Keys_UInt32_FreeRDP_DesktopHeight,
        ),
    );
    freerdp_settings_set_uint32(
        (*(*client).context).settings,
        FreeRDP_Settings_Keys_UInt32_FreeRDP_DesktopWidth,
        (*ctx).width,
    );
    freerdp_settings_set_uint32(
        (*(*client).context).settings,
        FreeRDP_Settings_Keys_UInt32_FreeRDP_DesktopHeight,
        (*ctx).height,
    );
    // Post-negotiation keyboard dump (WAYRDP_DEBUG_INPUT): by now ClientInfo
    // (TS_UD_CS_CORE) and the client's ConfirmActive TS_INPUT_CAPABILITYSET
    // have been applied into settings, so these read back what the client
    // advertised — the difference between vmconnect (dead keyboard) and
    // xfreerdp (works) is visible here. Diagnostic; gated to keep routine
    // connection logs quiet.
    if std::env::var_os("WAYRDP_DEBUG_INPUT").is_some() {
        let st = (*(*client).context).settings;
        eprintln!(
            "[peer_post_connect] input caps: FastPathInput={} UnicodeInput={} \
             KbdType=0x{:x} KbdSubType=0x{:x} KbdFnKeys={} KbdLayout=0x{:x}",
            freerdp_settings_get_bool(st, FreeRDP_Settings_Keys_Bool_FreeRDP_FastPathInput),
            freerdp_settings_get_bool(st, FreeRDP_Settings_Keys_Bool_FreeRDP_UnicodeInput),
            freerdp_settings_get_uint32(st, FreeRDP_Settings_Keys_UInt32_FreeRDP_KeyboardType),
            freerdp_settings_get_uint32(st, FreeRDP_Settings_Keys_UInt32_FreeRDP_KeyboardSubType),
            freerdp_settings_get_uint32(
                st,
                FreeRDP_Settings_Keys_UInt32_FreeRDP_KeyboardFunctionKey
            ),
            freerdp_settings_get_uint32(st, FreeRDP_Settings_Keys_UInt32_FreeRDP_KeyboardLayout),
        );
    }
    eprintln!(
        "[peer_post_connect] negotiated {}x{}",
        (*ctx).width,
        (*ctx).height
    );

    // Open the backend session now that the desktop size is negotiated.
    // ServerState lives in ContextExtra (put there by peer_accepted).
    let state = (*client).ContextExtra as *mut ServerState;
    if state.is_null() {
        return 0;
    }
    let state = &mut *state;
    let size = Size {
        width: (*ctx).width,
        height: (*ctx).height,
    };
    // Per-client presentation prefs from TS_UD_CS_CORE (applied into
    // settings by ClientInfo, so post_connect reads them reliably): the
    // LCID the backend maps to an XKB layout, and the host's desktop scale
    // percent the backend maps to an output scale. Both default to 0/100 on
    // clients that don't send them; backends must tolerate 0 as "unknown".
    let st = (*(*client).context).settings;
    let req = SessionRequest {
        initial_size: size,
        target: CaptureTarget::NewVirtualOutput(size),
        keyboard_layout: freerdp_settings_get_uint32(
            st,
            FreeRDP_Settings_Keys_UInt32_FreeRDP_KeyboardLayout,
        ),
        desktop_scale_pct: freerdp_settings_get_uint32(
            st,
            FreeRDP_Settings_Keys_UInt32_FreeRDP_DesktopScaleFactor,
        ),
    };
    let mut session = match (state.open_session)(req) {
        Some(s) => s,
        None => return 0,
    };
    let (tx, rx) = channel::<Frame>();
    // Metadata-cursor relay: the backend publishes the latest SESSION-space
    // cursor position into this slot; the mainloop forwards it newest-wins
    // as PTR_POS_UPDATE. OFF BY DEFAULT (WAYRDP_POINTER_METADATA): the relay
    // is half of Metadata mode — until the cursor SHAPE is also sent (and
    // Hyprland's cursor on the captured output hidden), the client still
    // draws its transparent painted-cursor shape, so the packet has no
    // visible effect on mstsc/vmconnect, while FreeRDP clients WARP THE
    // LOCAL CURSOR on PTR_POS_UPDATE: echoing injected motion back at wire
    // latency makes the xfreerdp cursor fight the user's hand.
    // Client-drawn cursor (WAYRDP_CLIENT_CURSOR): the client draws the
    // compositor's cursor image itself, so the pointer moves at local speed
    // and the video carries no cursor (no lag trail, no second pointer).
    // Off by default until the T4 pointer tests pass on vmconnect.
    let cursor_slot: Option<desk_backend::PointerShapeSlot> =
        if std::env::var_os("WAYRDP_CLIENT_CURSOR").is_some() {
            let slot: desk_backend::PointerShapeSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
            if session.set_pointer_shape_slot(slot.clone()) {
                eprintln!("[peer_post_connect] client-drawn cursor on");
                Some(slot)
            } else {
                eprintln!("[peer_post_connect] backend cannot deliver cursor images; cursor stays painted");
                None
            }
        } else {
            None
        };
    // No position echo with a client-drawn cursor: the client already moves
    // its own pointer, and FreeRDP clients warp on every PTR_POS_UPDATE.
    let pos_slot: Option<desk_backend::PointerPositionSlot> =
        if cursor_slot.is_none() && std::env::var_os("WAYRDP_POINTER_METADATA").is_some() {
            let slot: desk_backend::PointerPositionSlot =
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            session.set_pointer_position_slot(slot.clone());
            Some(slot)
        } else {
            None
        };

    // e6: WinPR event signalled after every frame send so the mainloop's
    // wait returns immediately on a new frame instead of at the next 16ms
    // tick. The closure below is what the backend pump thread calls: it must
    // only SetEvent (cheap, non-blocking) — never block the pump on the
    // protocol thread.
    (*ctx).frame_event = wayrdp_create_event();
    if !(*ctx).frame_event.is_null() {
        let ev = (*ctx).frame_event as usize;
        let wake: desk_backend::FrameWake = std::sync::Arc::new(move || unsafe {
            wayrdp_set_event(ev as HANDLE);
        });
        session.set_frame_wake(wake);
    } else {
        eprintln!("[peer_post_connect] frame event creation failed; falling back to frame polling");
    }

    if session.start(tx).is_err() {
        eprintln!("[peer_post_connect] session.start failed");
        return 0;
    }
    (*ctx).session = Some(session);
    (*ctx).frame_rx = Some(rx);
    (*ctx).pointer_pos_slot = pos_slot;
    (*ctx).cursor_slot = cursor_slot;
    eprintln!("[peer_post_connect] session started ok");

    // F3: slow-path frame back-pressure is released by SurfaceFrameAcknowledge
    // (the client sends it for every END marker); without the callback the
    // session freezes after its first frame.
    let update = (*(*client).context).update;
    if !update.is_null() {
        (*update).SurfaceFrameAcknowledge = Some(peer_surface_frame_acknowledge);
    }

    // D4: EGFX. When the client negotiated the Graphics Pipeline (mstsc
    // does; VMConnect basic doesn't), create the rdpgfx server channel.
    // Shadow's ordering: context_new + Initialize here, callbacks + Open
    // only once the mainloop sees drdynvc reach DRDYNVC_STATE_READY — the
    // DVC open needs that static channel up first.
    let gfx_offered = freerdp_settings_get_bool(
        (*(*client).context).settings,
        FreeRDP_Settings_Keys_Bool_FreeRDP_SupportGraphicsPipeline,
    ) != 0;
    if gfx_offered {
        let rdpgfx = rdpgfx_server_context_new((*ctx).vcm);
        if rdpgfx.is_null() {
            eprintln!("[peer_post_connect] rdpgfx_server_context_new failed; staying slow-path");
        } else {
            wayrdp_rdpgfx_set_rdpcontext(rdpgfx, ctx as *mut _);
            (*rdpgfx).custom = ctx as *mut _;
            (*rdpgfx).CapsAdvertise = Some(peer_rdpgfx_caps_advertise);
            (*rdpgfx).FrameAcknowledge = Some(peer_rdpgfx_frame_acknowledge);
            // externalThread=TRUE: our mainloop pumps handle_messages
            // instead of rdpgfx spawning its own thread.
            // Initialize returns WinPR BOOL: nonzero = success (unlike the
            // send PDUs which return UINT error codes).
            if wayrdp_rdpgfx_call_initialize(rdpgfx, 1) == 0 {
                eprintln!("[peer_post_connect] rdpgfx Initialize failed; staying slow-path");
                rdpgfx_server_context_free(rdpgfx);
            } else {
                (*ctx).rdpgfx = rdpgfx;
                eprintln!("[peer_post_connect] rdpgfx created; Open deferred to drdynvc ready");
            }
        }
    }

    // D5: Display-Control (RDPEDISP). Advertise support when the client
    // asked for it, so window resizes arrive as monitor-layout PDUs instead
    // of forcing reconnects. Open is deferred to drdynvc READY like rdpgfx.
    let disp_offered = freerdp_settings_get_bool(
        (*(*client).context).settings,
        FreeRDP_Settings_Keys_Bool_FreeRDP_SupportDisplayControl,
    ) != 0;
    if disp_offered {
        let disp = disp_server_context_new((*ctx).vcm);
        if disp.is_null() {
            eprintln!("[peer_post_connect] disp_server_context_new failed; no resize support");
        } else {
            (*disp).custom = ctx as *mut _;
            (*disp).MaxNumMonitors = 16;
            (*disp).MaxMonitorAreaFactorA = 4096;
            (*disp).MaxMonitorAreaFactorB = 2048;
            (*disp).DispMonitorLayout = Some(peer_disp_monitor_layout);
            (*ctx).disp = disp;
            eprintln!("[peer_post_connect] disp created; Open deferred to drdynvc ready");
        }
    }

    // Clipboard (MS-RDPECLIP) rides the cliprdr STATIC channel, joined at
    // connect time, so it starts here — no drdynvc wait. Shadow does the
    // same in its post_connect.
    if wayrdp_vcm_is_channel_joined((*ctx).vcm, wayrdp_cliprdr_channel_name()) != 0 {
        match (state.open_clipboard)() {
            Some(clipboard) => {
                (*ctx).cliprdr =
                    crate::cliprdr::Cliprdr::start((*ctx).vcm, ctx as *mut rdpContext, clipboard);
            }
            None => eprintln!("[peer_post_connect] backend offers no clipboard; cliprdr not served"),
        }
    }
    1
}

unsafe extern "C" fn peer_activate(client: *mut rdp_freerdp_peer) -> BOOL {
    match ffi_guard(|| {
        eprintln!("[peer_activate] enter");
        let ctx = (*client).context as *mut PeerContext;
        (*ctx).activated = true;
        // First-send + re-send cadence: the client's pointer state resets on
        // EGFX reset / resize, so the transparent shape must go out after
        // activation and again from the frame loop.
        if !send_pointer_state(ctx, (*ctx).base.update) {
            (*ctx).pointer_resend = true;
        }
        1
    }) {
        Some(rc) => rc,
        None => 0,
    }
}

/// DISPLAY_CONTROL_MONITOR_LAYOUT (MS-RDPEDISP 2.2.2.2): the client's new
/// monitor geometry — initial layout after caps plus every window resize.
/// Runs on the disp reader thread, so it only records the request; the
/// mainloop applies it (session.resize + gfx re-surface) on its own thread.
unsafe extern "C" fn peer_disp_monitor_layout(
    context: *mut s_disp_server_context,
    pdu: *const DISPLAY_CONTROL_MONITOR_LAYOUT_PDU,
) -> UINT {
    match ffi_guard(|| peer_disp_monitor_layout_inner(context, pdu)) {
        Some(rc) => rc,
        None => 1,
    }
}

unsafe fn peer_disp_monitor_layout_inner(
    context: *mut s_disp_server_context,
    pdu: *const DISPLAY_CONTROL_MONITOR_LAYOUT_PDU,
) -> UINT {
    if context.is_null() || pdu.is_null() {
        return 1;
    }
    let ctx = (*context).custom as *mut PeerContext;
    if ctx.is_null() {
        return 1;
    }
    let n = (*pdu).NumMonitors;
    if n == 0 || (*pdu).Monitors.is_null() {
        eprintln!("[disp_monitor_layout] 0 monitors; ignoring");
        return 0;
    }
    // Single-monitor desktop: take monitor 0's extents as the new size.
    // Multi-monitor layouts are a later milestone (D6+).
    let m = &*(*pdu).Monitors;
    eprintln!(
        "[disp_monitor_layout] monitor[0]: {}x{} @ ({},{}) phys {}x{} orient {}",
        m.Width, m.Height, m.Left, m.Top, m.PhysicalWidth, m.PhysicalHeight, m.Orientation
    );
    // Clamp immediately at the trust boundary (S2): the values below feed
    // backend resize() and UINT16 gfx surface fields.
    let (w, h) = clamp_size(m.Width, m.Height);
    if w != m.Width || h != m.Height {
        eprintln!(
            "[disp_monitor_layout] clamped client geometry {}x{} -> {w}x{h}",
            m.Width, m.Height
        );
    }
    // Lock-then-store every request; the mainloop consumes it in
    // apply_disp_resize. Do NOT compare against (*ctx).width/height here:
    // those belong to the mainloop thread, and a quick A->B->A sequence
    // seen before the first apply reads the stale size and drops B. The
    // no-op filter runs in apply_disp_resize on the owning thread.
    // Tolerate poisoning: a panicking writer must not wedge resizes.
    let mut guard = (*ctx).disp_req.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Size {
        width: w,
        height: h,
    });
    0
}

/// Apply a pending Display-Control resize on the mainloop thread: resize
/// the backend session, forget the gfx surface (the next frame re-runs
/// ResetGraphics -> CreateSurface -> MapSurfaceToOutput at the new size).
/// Slow-path sessions pick the new size up naturally from the frames.
unsafe fn apply_disp_resize(ctx: *mut PeerContext) {
    let pending = {
        let mut guard = (*ctx).disp_req.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    let Some(req) = pending else {
        return;
    };
    // No-op filter lives here on the mainloop thread (the disp reader
    // stores every request): drop requests that match the current size,
    // e.g. the tail of a quick A->B->A once B was already applied.
    if req.width == (*ctx).width && req.height == (*ctx).height {
        eprintln!("[disp_resize] requested size equals current; skipping");
        return;
    }
    let w = req.width;
    let h = req.height;
    eprintln!(
        "[disp_resize] applying {w}x{h} (was {}x{})",
        (*ctx).width,
        (*ctx).height
    );
    if let Some(session) = (*ctx).session.as_mut() {
        match session.resize(req) {
            Ok(()) => {
                (*ctx).width = w;
                (*ctx).height = h;
                // Force full gfx re-surface; rfx encoder is recreated inside
                // rdpgfx_recreate_surface.
                if (*ctx).gfx_surface_exists {
                    (*ctx).gfx_surface_exists = false;
                }
                eprintln!("[disp_resize] session resized to {w}x{h}");
            }
            Err(e) => {
                eprintln!(
                    "[disp_resize] backend resize failed: {e}; keeping {}x{}",
                    (*ctx).width,
                    (*ctx).height
                );
            }
        }
    } else {
        (*ctx).width = w;
        (*ctx).height = h;
    }
}

/// Outcome of the mainloop's `recv(MSG_PEEK | MSG_DONTWAIT)` EOF probe.
#[derive(Debug, PartialEq, Eq)]
enum EofPeek {
    /// Data pending or merely idle (EAGAIN/EINTR): the peer is alive.
    Alive,
    /// Orderly shutdown: a 0-byte read on a stream socket.
    Eof,
    /// The transport cannot peek. hv_sock (the Hyper-V vsock transport
    /// vmconnect uses) has no way to peek its ring buffer and fails MSG_PEEK
    /// with EOPNOTSUPP whenever data is pending — a healthy, busy socket.
    /// Treating it as an error dropped live sessions within seconds at
    /// 1920x1200; ask [`socket_hung_up`] instead.
    PeekUnsupported,
    /// A hard socket error.
    Error(i32),
}

fn eof_peek_verdict(r: isize, err: i32) -> EofPeek {
    if r > 0 {
        EofPeek::Alive
    } else if r == 0 {
        EofPeek::Eof
    } else if err == libc::EOPNOTSUPP {
        EofPeek::PeekUnsupported
    } else if err == libc::EAGAIN || err == libc::EWOULDBLOCK || err == libc::EINTR {
        EofPeek::Alive
    } else {
        EofPeek::Error(err)
    }
}

/// Non-blocking hangup check that never reads: vsock reports a peer
/// shutdown as POLLRDHUP/POLLHUP, and pending data alone sets neither.
fn socket_hung_up(fd: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: one valid pollfd, zero timeout.
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    n > 0 && pfd.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

#[cfg(test)]
mod eof_probe_tests {
    use super::*;

    #[test]
    fn peek_unsupported_is_not_a_socket_error() {
        // Regression: hv_sock's EOPNOTSUPP tore down live vmconnect sessions.
        assert_eq!(eof_peek_verdict(-1, libc::EOPNOTSUPP), EofPeek::PeekUnsupported);
    }

    #[test]
    fn peek_verdicts() {
        assert_eq!(eof_peek_verdict(1, 0), EofPeek::Alive);
        assert_eq!(eof_peek_verdict(0, 0), EofPeek::Eof);
        assert_eq!(eof_peek_verdict(-1, libc::EAGAIN), EofPeek::Alive);
        assert_eq!(eof_peek_verdict(-1, libc::EINTR), EofPeek::Alive);
        assert_eq!(eof_peek_verdict(-1, libc::ECONNRESET), EofPeek::Error(libc::ECONNRESET));
    }

    #[test]
    fn hangup_check_ignores_pending_data_and_sees_peer_close() {
        let mut fds = [0i32; 2];
        // SAFETY: plain socketpair/write/close on fds owned by this test.
        unsafe {
            assert_eq!(libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()), 0);
            assert!(!socket_hung_up(fds[0]), "idle, open peer");
            assert_eq!(libc::write(fds[1], b"x".as_ptr() as *const libc::c_void, 1), 1);
            assert!(!socket_hung_up(fds[0]), "pending data from an open peer is not a hangup");
            libc::close(fds[1]);
            assert!(socket_hung_up(fds[0]), "peer closed");
            libc::close(fds[0]);
        }
    }
}

unsafe fn peer_mainloop(client: *mut rdp_freerdp_peer, ctx: *mut PeerContext) {
    eprintln!("[peer_mainloop] enter");
    let mut iter: u32 = 0;
    // e12: capture-stamp→mainloop-send latency window, logged ~5s under
    // WAYRDP_DEBUG_CAPTURE. This is the mainloop's share of glass-to-glass:
    // channel queue wait + rdpgfx wait pacing + encode enqueue.
    let mut lat_n: u64 = 0;
    let mut lat_sum: u64 = 0;
    let mut lat_max: u32 = 0;
    let mut lat_samples: Vec<u32> = Vec::new();
    let mut lat_last_log = std::time::Instant::now();
    // Wall-clock throttle anchor for the EOF probe (decoupled from the
    // wait-iteration cadence; start in the past so the first probe runs
    // promptly rather than waiting out a full interval).
    let mut last_eof_probe = std::time::Instant::now() - std::time::Duration::from_secs(2);
    // Wall-clock anchor for the activation deadline (fix 4).
    let loop_start = std::time::Instant::now();
    loop {
        iter = iter.wrapping_add(1);
        let mut handles: [HANDLE; 64] = [ptr::null_mut(); 64];
        let get = (*client).GetEventHandles.expect("client.GetEventHandles");
        let n = get(client, handles.as_mut_ptr(), handles.len() as DWORD);
        if n == 0 {
            break;
        }
        // F12: GetEventHandles returning the table size used to kill the
        // session, and the appends below could write past the array when the
        // table was exactly full. Clamp and bounds-check every append.
        if n as usize > handles.len() {
            eprintln!(
                "[peer_mainloop] GetEventHandles returned {n}, clamping to {}",
                handles.len()
            );
        }
        let mut count = (n as usize).min(handles.len());
        if !(*ctx).vcm.is_null() && count < handles.len() {
            let vcm_event = wayrdp_vcm_get_event((*ctx).vcm);
            if !vcm_event.is_null() {
                handles[count] = vcm_event;
                count += 1;
            }
        }
        let mut gfx_index: Option<usize> = None;
        if !(*ctx).rdpgfx.is_null() && count < handles.len() {
            let gfx_event = rdpgfx_server_get_event_handle((*ctx).rdpgfx);
            if !gfx_event.is_null() {
                gfx_index = Some(count);
                handles[count] = gfx_event;
                count += 1;
            }
        }
        // e6: frame-publish event — a pump-side frame send wakes the wait
        // immediately rather than at the next 16ms tick.
        let mut frame_index: Option<usize> = None;
        if !(*ctx).frame_event.is_null() && count < handles.len() {
            frame_index = Some(count);
            handles[count] = (*ctx).frame_event;
            count += 1;
        }

        // e6: the frame-publish event is in the wait set, so a pump send
        // wakes this immediately; the 16ms timeout is now only a defensive
        // backstop (and the idle-loop cadence), not the frame latency.
        let rc = wayrdp_wait_multiple(count as DWORD, handles.as_ptr(), 16);
        if rc == wait_failed() {
            break;
        }
        // Manual-reset event: it wakes the wait, stays set, and we re-arm
        // it here after consuming the wakeup. Resetting before the drain
        // below is fine — the drain is try_recv-based and tolerates a
        // spurious wake; a frame sent between drain and reset just leaves
        // the event set, causing one immediate extra loop iteration.
        if let Some(i) = frame_index {
            if rc == wait_object_0() + i as DWORD {
                wayrdp_reset_event((*ctx).frame_event);
            }
        }

        if ((*client)
            .CheckFileDescriptor
            .expect("client.CheckFileDescriptor"))(client)
            == 0
        {
            break;
        }
        if !(*ctx).vcm.is_null() && wayrdp_vcm_check_fd((*ctx).vcm) == 0 {
            break;
        }

        // EOF probe. An abrupt client kill (SIGKILL, popup closed hard) leaves
        // the vsock in CLOSE_WAIT — the peer already sent its FIN — yet this
        // mainloop KEEPS SPINNING: the transport handle stays in the wait set
        // and fires forever, and FreeRDP 3.32's CheckFileDescriptor returns TRUE
        // on a socket at EOF while the session is still activating, so the
        // loop never breaks and the pump holds its lease. Peek one byte: a 0
        // return is an unambiguous end-of-stream on a stream socket. MSG_PEEK
        // leaves FreeRDP's own reads untouched; MSG_DONTWAIT keeps it off the
        // hot path; a ~1s throttle keeps it rare during normal traffic.
        if (*ctx).peer_fd >= 0 && last_eof_probe.elapsed() >= std::time::Duration::from_secs(1) {
            last_eof_probe = std::time::Instant::now();
            let mut b: [u8; 1] = [0; 1];
            let r = libc::recv(
                (*ctx).peer_fd,
                b.as_mut_ptr() as *mut libc::c_void,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            );
            let state = if (*ctx).activated {
                "activated"
            } else {
                "activating"
            };
            let err = if r < 0 { *libc::__errno_location() } else { 0 };
            match eof_peek_verdict(r, err) {
                EofPeek::Alive => {}
                EofPeek::Eof => {
                    eprintln!("[peer_mainloop] client closed connection during <{state}> (EOF on readable socket); tearing down");
                    break;
                }
                EofPeek::PeekUnsupported => {
                    if socket_hung_up((*ctx).peer_fd) {
                        eprintln!("[peer_mainloop] client closed connection during <{state}> (vsock hangup); tearing down");
                        break;
                    }
                }
                EofPeek::Error(err) => {
                    eprintln!(
                        "[peer_mainloop] peer socket error {err} during <{state}>; tearing down"
                    );
                    break;
                }
            }
        }
        // Activation deadline. Gates on the same `activated` flag the A3
        // harness gate reads, so daemon and harness agree on the meaning. A
        // client that never reaches ACTIVATE in time is stuck mid-handshake:
        // the EOF case above, or a wedged caps exchange the EOF probe can't
        // see. vmconnect activates in <2s; the default 30s is generous and
        // never trips a live session — an already-activated paused viewer is
        // unaffected. Tunable via WAYRDP_ACTIVATION_TIMEOUT_MS and logged when
        // it triggers, so a dropped slow-but-legitimate client shows up.
        if !(*ctx).activated
            && loop_start.elapsed() >= std::time::Duration::from_millis(activation_timeout_ms())
        {
            eprintln!("[peer_mainloop] activation deadline hit ({}ms); client never reached <activated>; tearing down", activation_timeout_ms());
            break;
        }
        // D4: drdynvc open/caps-req once the connection is ACTIVATED and the
        // channel actually joined (opening earlier send garbage PDUs into the
        // MCS handshake and mstsc resets the connection), then deferred
        // rdpgfx Open once the peer's capabilities response flips READY.
        let joined = !(*ctx).vcm.is_null()
            && wayrdp_vcm_is_channel_joined((*ctx).vcm, wayrdp_drdynvc_channel_name()) != 0;
        let state = if joined {
            wayrdp_vcm_drdynvc_state((*ctx).vcm)
        } else {
            0
        };
        // With a 16ms wait this keeps the gate log at roughly 1s cadence.
        if iter % 64 == 0 {
            eprintln!(
                "[peer_mainloop] gate: activated={} joined={} state={state}",
                (*ctx).activated as i32,
                joined as i32
            );
        }
        if (*ctx).activated && joined && state < wayrdp_drdynvc_state_ready() {
            let ok = wayrdp_vcm_open((*ctx).vcm);
            eprintln!("[peer_mainloop] vcm_open post-join -> {ok} (state was {state})");
        }
        if (*ctx).activated && !(*ctx).rdpgfx.is_null() && !(*ctx).gfx_opened {
            if state == wayrdp_drdynvc_state_ready() {
                if wayrdp_rdpgfx_call_open((*ctx).rdpgfx) == 0 {
                    eprintln!("[peer_mainloop] rdpgfx Open failed; disabling gfx");
                    rdpgfx_server_context_free((*ctx).rdpgfx);
                    (*ctx).rdpgfx = ptr::null_mut();
                } else {
                    (*ctx).gfx_opened = true;
                    eprintln!("[peer_mainloop] rdpgfx channel open");
                }
            }
        }
        // Ingress channel PDUs (caps advertise, frame acks) queue to this
        // event; handling them runs our CapsAdvertise/FrameAcknowledge
        // callbacks on this thread (externalThread=TRUE).
        if let Some(i) = gfx_index {
            if rc == wait_object_0() + i as DWORD {
                let res = rdpgfx_server_handle_messages((*ctx).rdpgfx);
                eprintln!("[peer_mainloop] rdpgfx_handle_messages -> {res}");
                if res != 0 {
                    break;
                }
            }
        }

        // D5: deferred disp Open rides the same drdynvc-READY gate as
        // rdpgfx; its reader thread then delivers monitor layouts.
        if (*ctx).activated && !(*ctx).disp.is_null() && !(*ctx).disp_opened {
            if state == wayrdp_drdynvc_state_ready() {
                // Open() spawns the disp reader thread; on success send
                // caps so the client starts sending monitor layouts.
                let drc = wayrdp_disp_call_open((*ctx).disp);
                if drc == 0 {
                    let caps = wayrdp_disp_call_caps((*ctx).disp);
                    (*ctx).disp_opened = drc == 0 && caps == 0;
                    eprintln!("[peer_mainloop] disp open rc={drc} caps={caps}");
                } else {
                    eprintln!("[peer_mainloop] disp Open failed rc={drc}; no resize support");
                }
            }
        }
        apply_disp_resize(ctx);

        if (*ctx).activated {
            // Metadata-cursor relay (only wired when WAYRDP_POINTER_METADATA
            // was set at post_connect): poll the shared slot, forward on
            // actual change — position packets are cheaper than frames but
            // not free.
            if let Some(slot) = (*ctx).pointer_pos_slot.as_ref() {
                let raw = slot.load(std::sync::atomic::Ordering::Relaxed);
                if raw != 0 && raw != (*ctx).pointer_pos_last {
                    (*ctx).pointer_pos_last = raw;
                    let x = (raw >> 32) as u32;
                    let y = (raw & 0xffff_ffff) as u32;
                    let update = (*client)
                        .context
                        .as_ref()
                        .map(|c| c.update)
                        .unwrap_or(ptr::null_mut());
                    if !update.is_null() {
                        let _ =
                            pointer::send_pointer_position(update, ctx as *mut rdpContext, x, y);
                    }
                }
            }

            // Client-drawn cursor: forward the newest shape (or hide) as soon
            // as the backend publishes it; shapes change rarely.
            let cursor_next = (*ctx)
                .cursor_slot
                .as_ref()
                .and_then(|slot| slot.lock().unwrap_or_else(|p| p.into_inner()).take());
            if let Some(next) = cursor_next {
                (*ctx).cursor_last = Some(next);
                let update = (*ctx).base.update;
                if !update.is_null() && !send_pointer_state(ctx, update) {
                    (*ctx).pointer_resend = true;
                }
            }

            // Drain all queued frames and only send the last one. F9: when
            // the backend capture pipeline dies, its sender is dropped and
            // try_iter would spin on an empty, dead channel forever — a
            // frozen black screen. Surface Disconnected and drop the peer.
            let Some(rx) = (*ctx).frame_rx.take() else {
                continue;
            };
            let mut latest: Option<Frame> = None;
            let mut backend_dead = false;
            loop {
                match rx.try_recv() {
                    Ok(f) => latest = Some(f),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        backend_dead = true;
                        break;
                    }
                }
            }
            (*ctx).frame_rx = Some(rx);
            if backend_dead {
                eprintln!("[peer_mainloop] backend frame producer gone; disconnecting peer");
                break;
            }
            if let Some(frame) = latest {
                // e12: the frame's capture stamp lets us measure publish→
                // encode-queue latency (unstamped frames: capture_stamp_ms=0).
                let stamp = frame.meta.capture_stamp_ms;
                if stamp != 0 {
                    let d = now_mono_ms_u32().wrapping_sub(stamp);
                    if std::env::var_os("WAYRDP_DEBUG_CAPTURE").is_some() {
                        lat_n += 1;
                        lat_sum += d as u64;
                        lat_max = lat_max.max(d);
                        lat_samples.push(d);
                        if lat_last_log.elapsed().as_secs() >= 5 {
                            lat_samples.sort_unstable();
                            let p95 = lat_samples
                                [(lat_samples.len() * 95 / 100).min(lat_samples.len() - 1)];
                            eprintln!(
                                "[gfx-lat] pub→enc_ms avg={} p95={} max={} over {}",
                                lat_sum / lat_n,
                                p95,
                                lat_max,
                                lat_n
                            );
                            lat_n = 0;
                            lat_sum = 0;
                            lat_max = 0;
                            lat_samples.clear();
                            lat_last_log = std::time::Instant::now();
                        }
                    }
                }
                if (*ctx).gfx_ready && !(*ctx).rdpgfx.is_null() {
                    send_frame_gfx(ctx, &frame);
                } else {
                    send_frame(client, ctx, &frame);
                }
            }
        }
    }

    eprintln!(
        "[peer_mainloop] teardown: mainloop exit after {} iters",
        iter
    );
    // Drop the capture session FIRST, before any FreeRDP teardown. Neither
    // frame_rx nor session touches a FreeRDP object, so this must not wait
    // behind Disconnect()/context_free. Closing the session's cmd channel
    // signals the capture pump to tear down and release the lease; doing it
    // here guarantees the pump exits even if the FreeRDP teardown that
    // follows stalls (rdpgfx/disp/VCM join on an abruptly-killed transport).
    (*ctx).frame_rx = None;
    (*ctx).session = None;
    eprintln!("[peer_mainloop] teardown: capture session dropped");
    ((*client).Disconnect.expect("client.Disconnect"))(client);
    freerdp_peer_context_free(client);
    freerdp_peer_free(client);
    eprintln!("[peer_mainloop] teardown: peer freed");
}

/// Client acknowledged a slow-path frame (TS_FRAME_ACKNOWLEDGE, arriving on
/// the update thread): release the one-frame back-pressure in send_frame.
/// Without this callback the first END marker latches pending_ack forever
/// and the session freezes after one frame (F3).
unsafe extern "C" fn peer_surface_frame_acknowledge(context: *mut rdpContext, ack: UINT32) -> BOOL {
    match ffi_guard(|| {
        let ctx = context as *mut PeerContext;
        if ctx.is_null() {
            return 1;
        }
        if let Some(pending) = (*ctx).pending_ack {
            // Accept the pending id or anything newer (wrapping compare).
            if ack.wrapping_sub(pending) < 0x8000_0000 {
                (*ctx).pending_ack = None;
            }
        }
        1
    }) {
        Some(rc) => rc,
        None => 1,
    }
}

/// (Re-)assert the client's pointer: the compositor's cursor image in
/// client-drawn mode, otherwise (or when hidden, or when the client cannot
/// take a 32 bpp shape) the transparent shape over the painted cursor.
///
/// # Safety
/// `ctx` must be the live peer context and `update` its `rdpUpdate`.
unsafe fn send_pointer_state(ctx: *mut PeerContext, update: *mut rdpUpdate) -> bool {
    if let Some(desk_backend::CursorUpdate::Shape(img)) = (*ctx).cursor_last.as_ref() {
        if pointer::send_cursor_image(update, ctx as *mut rdpContext, img) {
            return true;
        }
    }
    pointer::send_transparent_pointer(update, ctx as *mut rdpContext)
}

fn send_frame(client: *mut rdp_freerdp_peer, ctx: *mut PeerContext, frame: &Frame) {
    unsafe {
        if (*ctx).pending_ack.is_some() {
            return; // client still owes us a frame ack
        }

        let update = (*(*client).context).update;
        if update.is_null() {
            return;
        }
        if (*ctx).activated && (*ctx).pointer_resend {
            send_pointer_state(ctx, update);
            (*ctx).pointer_resend = false;
        }
        let Some(surface_bits) = (*update).SurfaceBits else {
            return;
        };
        let marker_ok = freerdp_settings_get_bool(
            (*(*client).context).settings,
            FreeRDP_Settings_Keys_Bool_FreeRDP_SurfaceFrameMarkerEnabled,
        ) != 0
            && (*update).SurfaceFrameMarker.is_some();

        if marker_ok {
            let marker = SURFACE_FRAME_MARKER {
                frameAction: frame_action_begin(),
                frameId: (*ctx).frame_id,
            };
            if ((*update).SurfaceFrameMarker.unwrap())((*client).context, &marker) == 0 {
                return;
            }
        }

        let header: TS_COMPRESSED_BITMAP_HEADER_EX = std::mem::zeroed();
        let cmd = SURFACE_BITS_COMMAND {
            cmdType: cmdtype_set_surface_bits(),
            destLeft: 0,
            destTop: 0,
            destRight: frame.meta.size.width as UINT32,
            destBottom: frame.meta.size.height as UINT32,
            bmp: TS_BITMAP_DATA_EX {
                bpp: 32,
                flags: 0,
                codecID: 0,
                width: frame.meta.size.width as UINT16,
                height: frame.meta.size.height as UINT16,
                bitmapDataLength: frame.pixels.len() as UINT32,
                exBitmapDataHeader: header,
                bitmapData: frame.pixels.as_ptr() as *mut BYTE,
            },
            skipCompression: 1,
        };
        let ok = surface_bits((*client).context, &cmd);

        if marker_ok {
            let fid = (*ctx).frame_id;
            let marker = SURFACE_FRAME_MARKER {
                frameAction: frame_action_end(),
                frameId: fid,
            };
            ((*update).SurfaceFrameMarker.unwrap())((*client).context, &marker);
            (*ctx).pending_ack = Some(fid);
        }
        if ok != 0 {
            (*ctx).frame_id = (*ctx).frame_id.wrapping_add(1);
        }
    }
}

// -------------------- D4: EGFX (Graphics Pipeline) --------------------

/// Our single fullscreen surface. Surface 0 is reserved by MS-RDPEGFX.
const GFX_SURFACE_ID: UINT16 = 1;

/// Client advertised its gfx capability sets; we must answer with exactly
/// one CapsConfirm. Mirrors shadow_rdpgfx's flow: pick the newest version,
/// negotiate THINCLIENT/SMALL_CACHE off (we manage nothing client-side),
/// force AVC_DISABLED (no H.264 encoder available in Phase 0).
///
/// Like shadow (`shadow_client_rdpgfx_caps_advertise`), we do NOT create or
/// destroy the surface here: mstsc sends a *second* CapsAdvertise once the
/// connection is fully up (display re-negotiation / monitor layout), and
/// tearing down the live surface in that handler makes mstsc reset the
/// connection (the "background paints, then protocol error" symptom).
/// Surface creation is deferred to the first gfx frame, guarded on
/// `gfx_surface_exists == false`.
unsafe extern "C" fn peer_rdpgfx_caps_advertise(
    context: *mut RdpgfxServerContext,
    pdu: *const RDPGFX_CAPS_ADVERTISE_PDU,
) -> UINT {
    match ffi_guard(|| peer_rdpgfx_caps_advertise_inner(context, pdu)) {
        Some(rc) => rc,
        None => 1,
    }
}

fn peer_rdpgfx_caps_advertise_inner(
    context: *mut RdpgfxServerContext,
    pdu: *const RDPGFX_CAPS_ADVERTISE_PDU,
) -> UINT {
    unsafe {
        if context.is_null() || pdu.is_null() {
            return 1;
        }
        eprintln!("[rdpgfx_caps_advertise] {} cap set(s)", (*pdu).capsSetCount);
        let ctx = (*context).custom as *mut PeerContext;
        let had_surface = !ctx.is_null() && (*ctx).gfx_surface_exists;
        let masks = RDPGFX_CAPS_FLAG_RDPGFX_CAPS_FLAG_THINCLIENT
            | RDPGFX_CAPS_FLAG_RDPGFX_CAPS_FLAG_SMALL_CACHE;
        let mut best: Option<RDPGFX_CAPSET> = None;
        for i in 0..(*pdu).capsSetCount as isize {
            let set = &*(*pdu).capsSets.offset(i);
            if best.map_or(true, |b| set.version > b.version) {
                best = Some(*set);
            }
        }
        let Some(mut set) = best else {
            eprintln!("[rdpgfx_caps_advertise] no usable cap set");
            return 1;
        };
        set.flags &= !masks;
        set.flags |= RDPGFX_CAPS_FLAG_RDPGFX_CAPS_FLAG_AVC_DISABLED;
        // capsDataLength = size of capsData = the flags field (4 bytes).
        // FreeRDP's server writer appends `length - 4` zero padding and sizes the
        // PDU as BASE(8) + length, but its client parser consumes only the 12-byte
        // version/length/flags triple — a length of 8 puts 4 stray bytes on the
        // wire and trips the client's "Unexpected gfx pdu end" check. mstsc
        // tolerates the padding; xfreerdp does not.
        set.length = 4;
        eprintln!(
            "[rdpgfx_caps_advertise] confirming version=0x{:08x} flags=0x{:08x}",
            set.version, set.flags
        );
        let mut confirm = RDPGFX_CAPS_CONFIRM_PDU { capsSet: &mut set };
        let rc = wayrdp_rdpgfx_call_caps_confirm(context, &mut confirm);
        if rc != 0 {
            eprintln!("[rdpgfx_caps_advertise] CapsConfirm write failed: {rc}");
            return rc;
        }
        if !ctx.is_null() {
            // CapsConfirm is the one point where the client restarts its
            // decoded/queue bookkeeping (mstsc's second CapsAdvertise
            // included); restart our send/ack counters with it. ResetGraphics
            // must NOT do this: the client keeps counting across it.
            (*ctx).gfx_frames_sent = 0;
            (*ctx).gfx_frames_acked = 0;
            (*ctx).gfx_ack_suspended = false;
            (*ctx).gfx_window_full_since = None;
            if had_surface {
                // Re-advertise with a live surface: leave client state alone and
                // just re-paint so the current frame is re-presented.
                request_repaint(ctx);
                eprintln!("[rdpgfx_caps_advertise] re-advertise; surface kept, repaint requested");
            } else {
                // Caps are confirmed; the EGFX frame path is now eligible and will
                // create the surface on the first frame.
                (*ctx).gfx_ready = true;
                eprintln!("[rdpgfx_caps_advertise] caps confirmed; surface created on first frame");
            }
        }
        0
    }
}

/// Ask the client to re-present the whole desktop (shadow's
/// `shadow_client_refresh_rect` equivalent). No-op if the update table or
/// the RefreshRect entry is unavailable.
unsafe fn request_repaint(ctx: *mut PeerContext) {
    if ctx.is_null() || (*ctx).peer.is_null() {
        return;
    }
    let update = (*(*(*ctx).peer).context).update;
    if update.is_null() {
        return;
    }
    let Some(refresh) = (*update).RefreshRect else {
        return;
    };
    let area = RECTANGLE_16 {
        left: 0,
        top: 0,
        right: (*ctx).width as UINT16,
        bottom: (*ctx).height as UINT16,
    };
    // Fullscreen single rect.
    refresh((*(*ctx).peer).context, 1, &area);
}

/// FrameAcknowledge keeps our outstanding-frame count honest (MS-RDPEGFX
/// 2.2.3.10). One PDU per decoded frame, so we count PDUs; the values
/// carried inside (queueDepth, totalFramesDecoded) are client-cumulative
/// and reset on CapsConfirm without notice, so they are unusable as a
/// gate.
///
/// queueDepth = 0xFFFFFFFF is SUSPEND_FRAME_ACKNOWLEDGEMENT (MS-RDPEGFX
/// 2.2.3.10, rdpgfx_main.c; xfreerdp's GfxSuspendFrameAck toggles it): the
/// client sends one such ack, then stops acknowledging entirely. `#define`d
/// in FreeRDP so bindgen dropped it; kept here with the spec reference.
const RDPGFX_SUSPEND_FRAME_ACKNOWLEDGEMENT: UINT32 = 0xFFFF_FFFF;

unsafe extern "C" fn peer_rdpgfx_frame_acknowledge(
    context: *mut RdpgfxServerContext,
    pdu: *const RDPGFX_FRAME_ACKNOWLEDGE_PDU,
) -> UINT {
    match ffi_guard(|| {
        if context.is_null() || pdu.is_null() {
            return 1;
        }
        let ctx = (*context).custom as *mut PeerContext;
        if ctx.is_null() {
            return 1;
        }
        if (*pdu).queueDepth == RDPGFX_SUSPEND_FRAME_ACKNOWLEDGEMENT {
            // Client will never acknowledge again; the window must not gate
            // (MS-RDPEGFX 2.2.3.10) or the pipeline freezes at 4 frames.
            (*ctx).gfx_ack_suspended = true;
        }
        (*ctx).gfx_frames_acked = (*ctx).gfx_frames_acked.wrapping_add(1);
        // Acks for frames sent before a CapsConfirm reset would push acked
        // above sent; saturating_sub hides that by leaving the window
        // permanently wider. Clamp instead.
        if (*ctx).gfx_frames_acked > (*ctx).gfx_frames_sent {
            (*ctx).gfx_frames_acked = (*ctx).gfx_frames_sent;
        }
        // Backlog is draining; the next full-window moment starts a fresh
        // timeout, so clears here.
        (*ctx).gfx_window_full_since = None;
        0
    }) {
        Some(rc) => rc,
        None => 1,
    }
}

/// Shadow-ordered surface bring-up: ResetGraphics -> CreateSurface ->
/// MapSurfaceToOutput -> (re)make the RFX encoder. ResetGraphics replaces
/// the client-side context/surface state, so we never send an explicit
/// DeleteSurface (mstsc treats a DeleteSurface it didn't ask for as a
/// protocol error and resets).
unsafe fn rdpgfx_recreate_surface(ctx: *mut PeerContext, width: UINT32, height: UINT32) -> BOOL {
    let rdpgfx = (*ctx).rdpgfx;
    if rdpgfx.is_null() || width == 0 || height == 0 {
        return 0;
    }
    let rrc = wayrdp_rdpgfx_call_reset_graphics(rdpgfx, width, height);
    eprintln!("[rdpgfx] ResetGraphics({}x{}) rc={rrc}", width, height);
    if rrc != 0 {
        eprintln!("[rdpgfx] ResetGraphics failed: {rrc}");
        return 0;
    }
    // ResetGraphics restarts the wire frame-ID space at 1. The client's
    // cumulative ack counters are NOT invalidated by it (only CapsConfirm
    // restarts those), so our gfx_frames_sent/acked stay untouched here.
    (*ctx).frame_id = 1;
    (*ctx).gfx_surface_exists = false;
    let create = RDPGFX_CREATE_SURFACE_PDU {
        surfaceId: GFX_SURFACE_ID,
        width: width as UINT16,
        height: height as UINT16,
        // Wire format constant (0x20); the FreeRDP-internal BGRX32 (0x88)
        // here makes clients abort the surface (verified via xfreerdp3:
        // "RecvCreateSurfacePdu ... pixelFormat: 0x88" -> OnClose).
        pixelFormat: wayrdp_gfx_pixel_format_xrgb_8888(),
    };
    let rc = wayrdp_rdpgfx_call_create_surface(rdpgfx, &create);
    eprintln!(
        "[rdpgfx] CreateSurface(id={} {}x{}) rc={rc}",
        create.surfaceId, width, height
    );
    if rc != 0 {
        eprintln!("[rdpgfx] CreateSurface failed: {rc}");
        return 0;
    }
    (*ctx).gfx_surface_exists = true;
    let rc = wayrdp_rdpgfx_call_map_to_output(rdpgfx, GFX_SURFACE_ID, 0, 0);
    eprintln!("[rdpgfx] MapSurfaceToOutput rc={rc}");
    if rc != 0 {
        eprintln!("[rdpgfx] MapSurfaceToOutput failed: {rc}");
        return 0;
    }
    if !(*ctx).rfx.is_null() {
        wayrdp_rfx_free((*ctx).rfx);
    }
    (*ctx).rfx = wayrdp_rfx_new(width, height);
    if (*ctx).rfx.is_null() {
        eprintln!("[rdpgfx] rfx encoder init failed");
        return 0;
    }
    1
}

/// Throttle corrective resizes so a drifting backend doesn't trigger one per
/// frame. Applied when the backend's frame size diverges from the negotiated
/// surface size.
const RESIZE_CORRECT_EVERY: u32 = 30;

/// EGFX flow-control window: max surface frames outstanding without a
/// FrameAcknowledge. Small enough to bound encode latency during fast
/// changes, large enough to cover one client decode batch.
// vmconnect (Hyper-V Enhanced Session) is a much slower acker than mstsc or
// FreeRDP clients; 4 frames in flight caps us at ~1-2 fps during normal use.
// MS-RDPEGFX §3.2.5.3.1 only *recommends* bounding in-flight frames to avoid
// unbounded buffering — the wire itself has no hard limit, and we have a
// GFX_ACK_TIMEOUT escape below if the client never acks.
const GFX_MAX_IN_FLIGHT: UINT32 = 60;

/// How long the window may stay full with no FrameAcknowledge before we
/// stop gating anyway: some clients disable frame acks entirely
/// (sendFrameAcks = FALSE) without sending SUSPEND_FRAME_ACKNOWLEDGEMENT,
/// and without this safety net the pipeline froze after 4 frames. Counted
/// from the moment the window filled, so a merely-idle connection keeps
/// gating normally.
const GFX_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Encode one frame as RemoteFX and queue it as a frame command.
fn send_frame_gfx(ctx: *mut PeerContext, frame: &Frame) {
    unsafe {
        let w = frame.meta.size.width;
        let h = frame.meta.size.height;
        if w != (*ctx).width || h != (*ctx).height {
            // Backend drifted from the negotiated size (frame != ctx). The
            // client surface is fixed at ctx.width x ctx.height, so rather
            // than dropping every frame (black screen), steer the backend
            // back to the negotiated size. Rate-limited to avoid a resize
            // storm while the backend transitions.
            (*ctx).gfx_resize_correct_ctr = (*ctx).gfx_resize_correct_ctr.wrapping_add(1);
            if (*ctx).gfx_resize_correct_ctr % RESIZE_CORRECT_EVERY == 1 {
                eprintln!(
                    "[send_frame_gfx] frame {w}x{h} != negotiated {}x{}; correcting backend size",
                    (*ctx).width,
                    (*ctx).height
                );
                let target = Size {
                    width: (*ctx).width as u32,
                    height: (*ctx).height as u32,
                };
                if let Some(session) = (*ctx).session.as_mut() {
                    if let Err(e) = session.resize(target) {
                        eprintln!("[send_frame_gfx] corrective resize failed: {e}");
                    }
                }
            }
            return;
        }
        if !matches!(frame.meta.format, desk_backend::PixelFormat::Bgra32) {
            return; // synthetic backend is always Bgra32 today
        }
        // Defer surface bring-up until the first gfx frame (shadow semantics):
        // only create after caps are confirmed *and* a frame is ready, so a
        // later re-CapsAdvertise never tears down a live surface.
        if !(*ctx).gfx_surface_exists {
            if rdpgfx_recreate_surface(ctx, w, h) == 0 {
                eprintln!("[send_frame_gfx] surface bring-up failed");
                return;
            }
            eprintln!("[send_frame_gfx] surface ready {w}x{h}");
            // EGFX ResetGraphics clears client pointer state; re-send the
            // transparent shape after the surface is back.
            if (*ctx).activated && !(*ctx).base.update.is_null() {
                if !send_pointer_state(ctx, (*ctx).base.update) {
                    (*ctx).pointer_resend = true;
                }
            }
        }
        // Flow control (F5, MS-RDPEGFX): fixed window over our own send/ack
        // counters. saturating_sub so late acks straddling a CapsConfirm
        // counter reset clamp to 0 instead of wrapping into a 4-billion
        // "debt" that would freeze the pipeline. Two escape hatches: the
        // client may suspend acks explicitly (gfx_ack_suspended, set when a
        // FrameAcknowledge carries SUSPEND_FRAME_ACKNOWLEDGEMENT), and a
        // never-acking client trips the GFX_ACK_TIMEOUT safety net.
        let unacked = (*ctx)
            .gfx_frames_sent
            .saturating_sub((*ctx).gfx_frames_acked);
        if unacked >= GFX_MAX_IN_FLIGHT && !(*ctx).gfx_ack_suspended {
            // Time from when the window filled, not from the last ack —
            // a client with acks disabled entirely (sendFrameAcks=FALSE)
            // has never acked, so stamp from the first blocking send if
            // no timestamp exists yet.
            let full_since = (*ctx)
                .gfx_window_full_since
                .get_or_insert_with(Instant::now);
            let waited = full_since.elapsed();
            if waited < GFX_ACK_TIMEOUT {
                // Log throttled to once per ~200 blocked iterations so we can
                // see ack starvation in the journal without flooding it.
                static BLOCK_LOG_CTR: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let n = BLOCK_LOG_CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n % 200 == 0 {
                    eprintln!(
                        "[send_frame_gfx] ack-starved: unacked={} sent={} acked={} waited={:?}",
                        unacked,
                        (*ctx).gfx_frames_sent,
                        (*ctx).gfx_frames_acked,
                        waited
                    );
                }
                return; // client owes us acks; wait
            }
            eprintln!(
                "[send_frame_gfx] ack timeout: unacked={} waited={:?} — bypassing back-pressure",
                unacked, waited
            );
        }
        let mut data: *const BYTE = ptr::null();
        let mut len: UINT32 = 0;
        // Pack compositor-emitted damage rects (x,y,w,h per quad, u16 each)
        // for the region-aware encoder. **Note:** vmconnect's RFX decoder
        // rejects region-encoded frames we produce via `rfx_compose_message`
        // (WTSVirtualChannelRead fails right after a non-full region frame
        // is delivered). Until that interop issue is solved, keep passing
        // zero rects — full-surface encode — and leave the region-aware
        // path dormant. Tile-diff damage detection was prototyped and
        // identified the same interop failure (region count didn't matter).
        // WAYRDP_TILED_DIFF=1 enables the software tile-diff damage path:
        // diff 64x64 tiles of the current frame against the previous one
        // and pass the dirty-tile union as the damage region. Hyprland's
        // ext-image-copy reports full-surface damage regardless of actual
        // change, so compositor hints never populate packed_rects — this
        // flag is the only way to exercise the region-encode path under
        // vmconnect. Default OFF because every prior attempt ends in
        // WTSVirtualChannelRead failure on the client.
        static TILED_DIFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let tiled_diff =
            *TILED_DIFF.get_or_init(|| std::env::var_os("WAYRDP_TILED_DIFF").is_some());
        let mut packed_rects: Vec<u16> = Vec::with_capacity(frame.meta.damage.len() * 4);
        // Compositor damage only feeds the region-aware encoder under the
        // flag: a compositor that ever emits granular damage must not
        // silently activate the (still unresolved) region-encode path.
        if tiled_diff {
            for r in &frame.meta.damage {
                if r.width == 0 || r.height == 0 || r.x < 0 || r.y < 0 {
                    continue;
                }
                packed_rects.push(r.x.min(u16::MAX as i32) as u16);
                packed_rects.push(r.y.min(u16::MAX as i32) as u16);
                packed_rects.push(r.width.min(u16::MAX as u32) as u16);
                packed_rects.push(r.height.min(u16::MAX as u32) as u16);
            }
            // If the compositor only gave us a full-surface hint, drop it so
            // we pass the full-frame sentinel (zero rect) to the encoder. Same
            // output as before this change, but the plumbing stays intact for
            // a compositor that eventually emits real granular damage.
            if packed_rects.len() == 4
                && packed_rects[0] == 0
                && packed_rects[1] == 0
                && packed_rects[2] as u64 * packed_rects[3] as u64 == w as u64 * h as u64
            {
                packed_rects.clear();
            }
        }
        let diff_start = std::time::Instant::now();
        if tiled_diff {
            let sw = w as usize;
            let sh = h as usize;
            let stride = frame.meta.stride as usize;
            let prev = (*ctx)
                .prev_frame_pixels
                .get_or_insert_with(|| vec![0u8; stride * sh].into_boxed_slice());
            if prev.len() != stride * sh {
                *prev = vec![0u8; stride * sh].into_boxed_slice();
            }
            let tiles_x = sw.div_ceil(64);
            let tiles_y = sh.div_ceil(64);
            let mut min_tx = usize::MAX;
            let mut min_ty = usize::MAX;
            let mut max_tx = 0usize;
            let mut max_ty = 0usize;
            let mut dirty = 0usize;
            // Bail out as soon as we know we'll fall back to full-frame:
            // diffing the rest of the surface just to then encode full is
            // wasted passes over 9MB. Threshold matches the bail rule below.
            let bail_tiles = (tiles_x * tiles_y) / 2;
            'outer: for ty in 0..tiles_y {
                for tx in 0..tiles_x {
                    let x0 = tx * 64;
                    let y0 = ty * 64;
                    let x1 = (x0 + 64).min(sw);
                    let y1 = (y0 + 64).min(sh);
                    let mut changed = false;
                    'rows: for y in y0..y1 {
                        let row_off = y * stride + x0 * 4;
                        let row_len = (x1 - x0) * 4;
                        let cur = &frame.pixels[row_off..row_off + row_len];
                        let pre = &prev[row_off..row_off + row_len];
                        if cur != pre {
                            changed = true;
                            break 'rows;
                        }
                    }
                    if changed {
                        dirty += 1;
                        min_tx = min_tx.min(tx);
                        min_ty = min_ty.min(ty);
                        max_tx = max_tx.max(tx);
                        max_ty = max_ty.max(ty);
                        if dirty > bail_tiles {
                            break 'outer;
                        }
                    }
                }
            }
            // Update prev-frame with the current capture for the next diff.
            // Even on heavy-motion frames this MUST run — skipping it leaves
            // prev stale and the NEXT frame then appears dirty everywhere,
            // cascading permanent full-frame encodes.
            for y in 0..sh {
                let row_off = y * stride;
                let row_len = sw * 4;
                prev[row_off..row_off + row_len]
                    .copy_from_slice(&frame.pixels[row_off..row_off + row_len]);
            }
            // Bail to full-frame if more than half the surface changed
            // (encoder tile count explodes with scattered damage and the
            // region overhead outgrows the win).
            if dirty * 2 > tiles_x * tiles_y {
                packed_rects.clear();
            } else if dirty > 0 && min_tx != usize::MAX {
                // Merge dirty tiles into one bbox — single-rect region frames
                // produced the same vmconnect failure as multi-rect ones.
                let x = (min_tx * 64).min(sw) as u16;
                let y = (min_ty * 64).min(sh) as u16;
                let rw = (((max_tx + 1) * 64).min(sw) - min_tx * 64) as u16;
                let rh = (((max_ty + 1) * 64).min(sh) - min_ty * 64) as u16;
                packed_rects.clear();
                packed_rects.push(x);
                packed_rects.push(y);
                packed_rects.push(rw);
                packed_rects.push(rh);
            }
        }
        let encode_start = std::time::Instant::now();
        let encoded = wayrdp_rfx_encode_region(
            (*ctx).rfx,
            frame.pixels.as_ptr(),
            frame.meta.stride as UINT32,
            if packed_rects.is_empty() {
                ptr::null()
            } else {
                packed_rects.as_ptr()
            },
            packed_rects.len() / 4,
            &mut data,
            &mut len,
        );
        let encode_ms = encode_start.elapsed().as_millis() as u64;
        let diff_ms = diff_start.elapsed().as_millis() as u64 - encode_ms;
        if encoded == 0 {
            eprintln!("[send_frame_gfx] RFX encode failed");
            return;
        }
        if packed_rects.is_empty() {
            (*ctx).damage_full_frames += 1;
        }
        let fid = (*ctx).frame_id;
        if std::env::var_os("WAYRDP_DEBUG_GFX").is_some()
            && (fid < 6 || fid % 300 == 0 || encode_ms > 15)
        {
            let rects = packed_rects.len() / 4;
            let total_px: u64 = packed_rects
                .chunks_exact(4)
                .map(|q| q[2] as u64 * q[3] as u64)
                .sum();
            eprintln!(
                "[send_frame_gfx] fid={} rects={} dmg_px={} ({}x{}={}px) full={} len={} diff={}ms enc={}ms",
                fid,
                rects,
                total_px,
                w,
                h,
                w as u64 * h as u64,
                (*ctx).damage_full_frames,
                len,
                diff_ms,
                encode_ms
            );
        }
        let fid = (*ctx).frame_id;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as UINT32)
            .unwrap_or(0);
        let start = RDPGFX_START_FRAME_PDU {
            timestamp: ts,
            frameId: fid,
        };
        let end = RDPGFX_END_FRAME_PDU { frameId: fid };
        // destRect must match what the RFX payload actually encodes.
        // FreeRDP's RFX encoder bakes the input rects' absolute coordinates
        // into the tile grid (rfx.c: gridRelX/gridRelY) and the client
        // decoder then ADDS this surface command's left/top to each tile
        // (gdi/gfx.c → rfx.c: updateRect.left = left + tile->x). Shadow is
        // immune only because its EGFX path is always full-screen
        // (cmd.left = rect.x = 0). Therefore the surface command must stay
        // full-surface (0,0,w,h) even for region-encoded payloads — a
        // sub-surface destRect double-offsets the tiles, placing pixels at
        // (2x, 2y) on strict clients.
        let (s_left, s_top, s_right, s_bottom, s_w, s_h) = (0u32, 0u32, w, h, w, h);
        let cmd = RDPGFX_SURFACE_COMMAND {
            surfaceId: GFX_SURFACE_ID as UINT32,
            codecId: RDPGFX_CODECID_RDPGFX_CODECID_CAVIDEO,
            contextId: 0,
            format: pixel_format_bgrx32(),
            left: s_left,
            top: s_top,
            right: s_right,
            bottom: s_bottom,
            width: s_w,
            height: s_h,
            length: len,
            data: data as *mut BYTE,
            extra: ptr::null_mut(),
        };
        let rc = wayrdp_rdpgfx_call_surface_frame_command((*ctx).rdpgfx, &cmd, &start, &end);
        if rc != 0 {
            eprintln!("[send_frame_gfx] SurfaceFrameCommand failed: {rc}");
            return;
        }
        (*ctx).frame_id = (*ctx).frame_id.wrapping_add(1);
        (*ctx).gfx_frames_sent = (*ctx).gfx_frames_sent.wrapping_add(1);
        // gfx_frames_acked is advanced by each FrameAcknowledge PDU.
    }
}

// -------------------- input callbacks --------------------

/// CLOCK_MONOTONIC ms (u32, wraps) — the stamp domain shared with the
/// backend pump and the capture plugin's shm header (e12 latency
/// instrumentation). Stamping at PDU decode measures input→pump-drain and
/// publish→encode queues; correctness never depends on it.
fn now_mono_ms_u32() -> u32 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let ms = ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000;
    (ms & 0xffff_ffff) as u32
}

/// How long a peer may stay pre-ACTIVATE before the mainloop drops it
/// (activation deadline). Tunable via WAYRDP_ACTIVATION_TIMEOUT_MS; default
/// 30s. Parsed once and cached; invalid values fall back to the default.
fn activation_timeout_ms() -> u64 {
    static ONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| {
        std::env::var("WAYRDP_ACTIVATION_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(30_000)
    })
}

unsafe fn inject_input(ctx: *mut PeerContext, ev: InputEvent) -> BOOL {
    if ctx.is_null() {
        return 1;
    }
    let Some(session) = (*ctx).session.as_mut() else {
        return 1;
    };
    if std::env::var_os("WAYRDP_DEBUG_INPUT").is_some() {
        eprintln!("[input] {ev:?}");
    }
    let stamped = StampedInput {
        event: ev,
        stamp_ms: now_mono_ms_u32(),
    };
    if let Err(e) = session.inject(stamped) {
        eprintln!("[input] inject failed: {e}");
    }
    1
}

/// rdpInput.context points at our PeerContext (set by peer_context_new).
unsafe fn input_ctx(input: *mut rdpInput) -> *mut PeerContext {
    if input.is_null() {
        return ptr::null_mut();
    }
    (*input).context as *mut PeerContext
}

/// MS-RDP-BCGR 2.2.8.1.1.3.1.1.8: low 9 bits of the wheel flags carry the
/// signed rotation (units of 120). `#define`d in FreeRDP so bindgen dropped
/// it; kept here with the spec reference.
const PTR_FLAGS_WHEEL_ROTATION_MASK: UINT32 = 0x01ff;

/// MS-RDP-BCGR 2.2.8.1.1.3.1.1.5: toggleFlags bits of TS_SYNC_EVENT_PDU.
/// `#define`d in FreeRDP so bindgen dropped them; kept here with the spec
/// reference.
const TS_SYNC_SCROLL_LOCK: UINT32 = 0x01;
const TS_SYNC_NUM_LOCK: UINT32 = 0x02;
const TS_SYNC_CAPS_LOCK: UINT32 = 0x04;
const TS_SYNC_KANA_LOCK: UINT32 = 0x08;

unsafe extern "C" fn peer_input_sync(input: *mut rdpInput, flags: UINT32) -> BOOL {
    match ffi_guard(|| {
        let ctx = input_ctx(input);
        // Lock-key sync (TS_SYNC_EVENT_PDU): feed the backend's virtual
        // keyboard so its locked modifier group matches; mstsc sends this
        // on focus changes rather than on every toggle, so the backend
        // additionally tracks remote lock-key presses. Clients like
        // xfreerdp that never send sync leave the locked group at 0,
        // exactly what a compositor with no real keyboard sees anyway.
        let lock_bits = (flags
            & (TS_SYNC_SCROLL_LOCK | TS_SYNC_NUM_LOCK | TS_SYNC_CAPS_LOCK | TS_SYNC_KANA_LOCK))
            as u8;
        inject_input(ctx, InputEvent::LockKeys { lock_bits })
    }) {
        Some(rc) => rc,
        None => 1,
    }
}

unsafe extern "C" fn peer_input_keyboard(input: *mut rdpInput, flags: UINT16, code: UINT8) -> BOOL {
    match ffi_guard(|| {
        let ctx = input_ctx(input);
        // Pressed = KBD_FLAGS_RELEASE absent, NOT KBD_FLAGS_DOWN present:
        // fastpath_recv_input_event_scancode passes only RELEASE/EXTENDED
        // flags (a press carries flags=0; KBD_FLAGS_DOWN is never set), so
        // testing DOWN renders every fast-path key press a phantom release
        // and the whole keyboard dies while the mouse still works — the
        // exact vmconnect symptom. Testing RELEASE is correct for both
        // slow path (spec sets exactly one of DOWN/RELEASE per event) and
        // fast path.
        let pressed = (flags & (KBD_FLAGS_RELEASE as UINT16)) == 0;
        let mut scancode = code as UINT32;
        if (flags & (KBD_FLAGS_EXTENDED as UINT16)) != 0 {
            scancode |= 0x100; // InputEvent::Key extended-flag bit (desk-backend contract)
        }
        inject_input(ctx, InputEvent::Key { scancode, pressed })
    }) {
        Some(rc) => rc,
        None => 1,
    }
}

unsafe extern "C" fn peer_input_unicode(input: *mut rdpInput, flags: UINT16, code: UINT16) -> BOOL {
    match ffi_guard(|| {
        let ctx = input_ctx(input);
        // MS-RDP-BCGR 2.2.8.1.1.3.1.1.8: bit 0x8000 set = key release. Unicode
        // key releases carry no extra info for our backends; deliver presses only.
        if (flags & 0x8000) != 0 {
            return 1;
        }
        let Some(ch) = char::from_u32(code as u32) else {
            return 1;
        };
        inject_input(ctx, InputEvent::Unicode(ch))
    }) {
        Some(rc) => rc,
        None => 1,
    }
}

unsafe extern "C" fn peer_input_mouse(
    input: *mut rdpInput,
    flags: UINT16,
    x: UINT16,
    y: UINT16,
) -> BOOL {
    match ffi_guard(|| peer_input_mouse_inner(input, flags, x, y)) {
        Some(rc) => rc,
        None => 1,
    }
}

unsafe fn peer_input_mouse_inner(
    input: *mut rdpInput,
    flags: UINT16,
    x: UINT16,
    y: UINT16,
) -> BOOL {
    let ctx = input_ctx(input);
    let flags32 = flags as UINT32;
    if (flags32 & PTR_FLAGS_MOVE) != 0 {
        inject_input(
            ctx,
            InputEvent::PointerMotion {
                x: x as INT32,
                y: y as INT32,
            },
        );
    }
    let button = if (flags32 & PTR_FLAGS_BUTTON1) != 0 {
        Some(PointerButton::Left)
    } else if (flags32 & PTR_FLAGS_BUTTON2) != 0 {
        Some(PointerButton::Right)
    } else if (flags32 & PTR_FLAGS_BUTTON3) != 0 {
        Some(PointerButton::Middle)
    } else {
        None
    };
    if let Some(button) = button {
        let pressed = (flags32 & PTR_FLAGS_DOWN) != 0;
        inject_input(ctx, InputEvent::PointerButton { button, pressed });
    }
    // Wheel rotation: 9-bit signed units of 120 (MS-RDP-BCGR 2.2.8.1.1.3.1.1.8).
    if (flags32 & PTR_FLAGS_WHEEL) != 0 {
        let mut rot = (flags32 & PTR_FLAGS_WHEEL_ROTATION_MASK) as i32;
        if rot & 0x100 != 0 {
            rot |= !0x1ff;
        }
        inject_input(ctx, InputEvent::PointerAxis { dx: 0, dy: rot });
    }
    1
}

unsafe extern "C" fn peer_input_rel_mouse(
    _input: *mut rdpInput,
    _flags: UINT16,
    _x: INT16,
    _y: INT16,
) -> BOOL {
    // Relative-motion events (RemoteApp only). Not used; ack.
    1
}

unsafe extern "C" fn peer_input_extended_mouse(
    input: *mut rdpInput,
    flags: UINT16,
    _x: UINT16,
    _y: UINT16,
) -> BOOL {
    match ffi_guard(|| peer_input_extended_mouse_inner(input, flags)) {
        Some(rc) => rc,
        None => 1,
    }
}

unsafe fn peer_input_extended_mouse_inner(input: *mut rdpInput, flags: UINT16) -> BOOL {
    let ctx = input_ctx(input);
    let flags32 = flags as UINT32;
    if (flags32 & PTR_FLAGS_HWHEEL) != 0 {
        let mut rot = (flags32 & PTR_FLAGS_WHEEL_ROTATION_MASK) as i32;
        if rot & 0x100 != 0 {
            rot |= !0x1ff;
        }
        inject_input(ctx, InputEvent::PointerAxis { dx: rot, dy: 0 });
    }
    let button = if (flags32 & PTR_XFLAGS_BUTTON1) != 0 {
        Some(PointerButton::Back)
    } else if (flags32 & PTR_XFLAGS_BUTTON2) != 0 {
        Some(PointerButton::Forward)
    } else {
        None
    };
    if let Some(button) = button {
        let pressed = (flags32 & PTR_XFLAGS_DOWN) != 0;
        inject_input(ctx, InputEvent::PointerButton { button, pressed });
    }
    1
}
