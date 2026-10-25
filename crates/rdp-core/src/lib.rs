//! wayrdp RDP server core.
//!
//! Phase 0 target: listener -> TLS -> activate -> slow-path SurfaceBits of a
//! synthetic desktop, then EGFX + Display Control. The backend seam already
//! exists; the synthetic source implements `desk-backend` in `synthetic.rs`
//! so protocol code never knows the desktop is fake.

pub mod synthetic;

#[cfg(unix)]
pub mod clipformat;
#[cfg(freerdp_present)]
pub mod cliprdr;
#[cfg(freerdp_present)]
pub mod consts;
#[cfg(freerdp_present)]
pub mod peer;
#[cfg(freerdp_present)]
pub mod pointer;
#[cfg(freerdp_present)]
pub mod vsock;

use desk_backend::{CaptureSession, DesktopBackend, SessionRequest, Size};

/// Top-level server. `run` blocks and serves peers until shutdown.
///
/// The backend is boxed so the server can select a compositor backend at
/// runtime (WAYRDP_BACKEND) rather than at compile time.
pub struct Server {
    pub backend: Box<dyn DesktopBackend>,
    pub bind: std::net::SocketAddr,
    pub cert_path: String,
    pub key_path: String,
    /// Also listen for RDP over Hyper-V VMBus (AF_VSOCK) on the same port,
    /// so vmconnect Enhanced Session Mode works. Degrades gracefully to
    /// TCP-only when the guest kernel lacks hv_sock.
    pub vsock: bool,
    /// Allowed peer CIDs for vsock connections; empty = accept any CID.
    pub vsock_allow: Vec<u32>,
    /// Serve the classic TCP listener. Opt-in (WAYRDP_TCP=1) because,
    /// unlike the vsock channel, it has no host-proxy trust boundary.
    pub tcp: bool,
    /// winpr SAM file for NLA auth on TCP peers; must be `Some` when
    /// `tcp` is set (the binary fails closed on this invariant).
    pub sam_path: Option<String>,
}

impl Server {
    pub fn run(self) -> anyhow::Result<()> {
        #[cfg(freerdp_present)]
        {
            return self.run_freerdp();
        }
        #[cfg(not(freerdp_present))]
        {
            let _ = &self.backend;
            anyhow::bail!(
                "wayrdp built without FreeRDP 3 support; \
                 install freerdp3-dev and rebuild to serve."
            )
        }
    }

    /// Block on the FreeRDP listener until the process is killed.
    ///
    /// One thread runs the accept loop; `peer_accepted` spawns a per-peer
    /// mainloop thread that drives activation and the frame pump.
    #[cfg(freerdp_present)]
    fn run_freerdp(self) -> anyhow::Result<()> {
        use std::ffi::CString;

        anyhow::ensure!(
            self.tcp || self.vsock,
            "no transport enabled: either tcp (WAYRDP_TCP=1) or vsock must be on"
        );
        // Fail closed: a TCP listener without an NLA credential store would
        // be an unauthenticated desktop on the network.
        anyhow::ensure!(
            !self.tcp || self.sam_path.is_some(),
            "TCP serving requires provisioned NLA credentials (SAM file)"
        );

        let bind_ip = self.bind.ip().to_string();
        let bind_port = self.bind.port();
        let bind_c = CString::new(bind_ip).expect("bind ip CString");
        let cert_c = CString::new(self.cert_path.clone()).expect("cert CString");
        let key_c = CString::new(self.key_path.clone()).expect("key CString");
        let sam_c = self
            .sam_path
            .as_deref()
            .map(|p| CString::new(p).expect("sam path CString"));

        // The closure is Arc'd because each accepted peer needs to reopen
        // the same backend; `self` is consumed by `run`.
        let backend = std::sync::Arc::new(self.backend);
        let clip_backend = std::sync::Arc::clone(&backend);
        let open_clipboard = move || clip_backend.open_clipboard();
        let open_session = move |req: SessionRequest| -> Option<Box<dyn CaptureSession>> {
            match backend.open(req) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("[wayrdp] backend.open failed: {e}");
                    None
                }
            }
        };

        let state = Box::new(peer::ServerState {
            cert_path: cert_c,
            key_path: key_c,
            open_session: Box::new(open_session),
            open_clipboard: Box::new(open_clipboard),
            vsock_allow: self.vsock_allow,
            sam_path: sam_c,
        });

        unsafe {
            // Process-global: install FreeRDP's built-in server-side WTS API
            // table so WTSOpenServerA peers a real in-process VCM (without
            // this, the WTS* calls fall through to the FreeRDS socket stubs
            // and the VCM stays null — the D3 probe finding). Same call
            // sfreerdp/shadow do at startup; idempotent.
            anyhow::ensure!(
                freerdp_sys::wayrdp_register_wts_api() != 0,
                "WTSRegisterWtsApiFunctionTable(FreeRDP_InitWtsApi) failed"
            );

            let listener = freerdp_sys::freerdp_listener_new();
            anyhow::ensure!(!listener.is_null(), "freerdp_listener_new failed");
            // Hand the state to listener->info and leak it for the listener
            // lifetime. The process runs until killed, so this is fine for D3.
            (*listener).info = Box::into_raw(state) as *mut _;
            (*listener).PeerAccepted = Some(peer::peer_accepted);
            // TCP is opt-in (WAYRDP_TCP=1): vsock-only mode registers just
            // the AF_VSOCK socket via OpenFromSocket below and never binds
            // a TCP port at all.
            if self.tcp {
                let open = (*listener).Open.expect("listener.Open");
                anyhow::ensure!(
                    open(listener, bind_c.as_ptr(), bind_port) != 0,
                    "listener.Open on port {} failed",
                    bind_port,
                );
            }

            // VMBus transport: hand our own AF_VSOCK listening socket to the
            // same listener; its accept path is transport-generic and marks
            // AF_VSOCK peers local (skips hostname lookup). Graceful no-op
            // on kernels without hv_sock.
            if self.vsock {
                match vsock::create_listener(bind_port as u32) {
                    Some(fd) => {
                        let open_from =
                            (*listener).OpenFromSocket.expect("listener.OpenFromSocket");
                        anyhow::ensure!(
                            open_from(listener, fd) != 0,
                            "listener.OpenFromSocket (vsock fd {}) failed",
                            fd,
                        );
                    }
                    None => {} // vsock::create_listener already logged why
                }
            }

            loop {
                let mut handles: [freerdp_sys::HANDLE; 64] = [std::ptr::null_mut(); 64];
                let get = (*listener)
                    .GetEventHandles
                    .expect("listener.GetEventHandles");
                let n = get(
                    listener,
                    handles.as_mut_ptr(),
                    handles.len() as freerdp_sys::DWORD,
                );
                if n == 0 {
                    break;
                }
                let rc = freerdp_sys::wayrdp_wait_multiple(
                    n,
                    handles.as_ptr(),
                    freerdp_sys::wayrdp_infinite(),
                );
                if rc == freerdp_sys::wayrdp_wait_failed() {
                    break;
                }
                if ((*listener)
                    .CheckFileDescriptor
                    .expect("listener.CheckFileDescriptor"))(listener)
                    == 0
                {
                    break;
                }
            }

            ((*listener).Close.expect("listener.Close"))(listener);
            let info = (*listener).info;
            freerdp_sys::freerdp_listener_free(listener);
            let _ = Box::from_raw(info as *mut peer::ServerState);
        }
        Ok(())
    }
}

/// Convenience helper used by tests to build a 1280x800 virtual-output
/// SessionRequest. (Unused by the D3 listener path.)
#[allow(dead_code)]
pub fn default_session_request(size: Size) -> SessionRequest {
    SessionRequest::without_client(size)
}
