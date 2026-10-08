//! Hyper-V VMBus (AF_VSOCK) transport for vmconnect Enhanced Session Mode.
//!
//! Windows' Enhanced Session connects to a Linux guest over hv_sock port
//! 3389 instead of TCP. FreeRDP's accept path is transport-generic, so we
//! create the vsock listening socket ourselves and hand it to the listener
//! via `OpenFromSocket`; the only vsock-specific bits left are this bind
//! and the per-accept CID allowlist in `peer_accepted`.
//!
//! Constants mirror <linux/vm_sockets.h>; no libc/bindgen dependency.

pub const AF_VSOCK: i32 = 40;
pub const SOCK_STREAM: i32 = 1;
pub const SOCK_CLOEXEC: i32 = 0o2000000;
/// 0xFFFFFFFF — listen for connections from any CID.
pub const VMADDR_CID_ANY: u32 = u32::MAX;
/// 2 — the Hyper-V host partition (default allowlist entry).
pub const VMADDR_CID_HOST: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrVm {
    svm_family: u16,
    svm_reserved1: u16, // must be 0 (kernel enforces)
    svm_port: u32,
    svm_cid: u32,
    svm_zero: [u8; 4],
}

type SockLen = u32; // socklen_t on linux-x86_64
const SOL_SOCKET: i32 = 1;
const SO_REUSEADDR: i32 = 2;

extern "C" {
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn bind(fd: i32, addr: *const SockaddrVm, len: SockLen) -> i32;
    fn listen(fd: i32, backlog: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn getpeername(fd: i32, addr: *mut SockaddrVm, len: *mut SockLen) -> i32;
    fn setsockopt(fd: i32, level: i32, name: i32, val: *const i32, len: SockLen) -> i32;
}

fn errno() -> i32 {
    unsafe { *libc_errno_location() }
}

extern "C" {
    #[link_name = "__errno_location"]
    fn libc_errno_location() -> *mut i32;
}

/// Parse `WAYRDP_VSOCK_ALLOW`: comma-separated CIDs. Missing or unparseable
/// entries fall back to the default; an empty string means "allow any".
pub fn parse_allowlist() -> Vec<u32> {
    parse_allowlist_from(std::env::var("WAYRDP_VSOCK_ALLOW").ok().as_deref())
}

/// Pure core of [`parse_allowlist`], kept separate so tests don't have to
/// mutate process env (which races when tests run on parallel threads).
pub fn parse_allowlist_from(raw: Option<&str>) -> Vec<u32> {
    match raw {
        Some(raw) if !raw.trim().is_empty() => raw
            .split(',')
            .map(|s| s.trim().parse::<u32>())
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|_| {
                eprintln!("[wayrdp] malformed WAYRDP_VSOCK_ALLOW; defaulting to host-only");
                default_allowlist()
            }),
        Some(_) => Vec::new(), // empty string: no CID filtering
        None => default_allowlist(),
    }
}

pub fn default_allowlist() -> Vec<u32> {
    vec![VMADDR_CID_HOST]
}

/// Create a listening AF_VSOCK socket on `port`, or return None (with a log
/// line) when the kernel lacks vsock support (EAFNOSUPPORT/EPROTONOSUPPORT)
/// or the hv_sock transport is absent (ENODEV). Hard errors likewise degrade
/// to None — TCP keeps serving regardless.
pub fn create_listener(port: u32) -> Option<i32> {
    unsafe {
        let fd = socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC, 0);
        if fd < 0 {
            eprintln!(
                "[wayrdp] vsock unavailable (socket errno={}); TCP-only mode",
                errno()
            );
            return None;
        }
        let one: i32 = 1;
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_REUSEADDR,
            &one,
            std::mem::size_of::<i32>() as SockLen,
        );
        let addr = SockaddrVm {
            svm_family: AF_VSOCK as u16,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: VMADDR_CID_ANY,
            svm_zero: [0; 4],
        };
        if bind(fd, &addr, std::mem::size_of::<SockaddrVm>() as SockLen) != 0 {
            let e = errno();
            eprintln!(
                "[wayrdp] vsock bind on port {} failed (errno={}); TCP-only mode",
                port, e
            );
            close(fd);
            return None;
        }
        if listen(fd, 4) != 0 {
            let e = errno();
            eprintln!("[wayrdp] vsock listen failed (errno={}); TCP-only mode", e);
            close(fd);
            return None;
        }
        eprintln!("[wayrdp] vsock listener bound on port {}", port);
        Some(fd)
    }
}

/// CID of the peer at the other end of `fd`, if `fd` is a vsock socket.
pub fn peer_cid(fd: i32) -> Option<u32> {
    unsafe {
        let mut addr = SockaddrVm {
            svm_family: 0,
            svm_reserved1: 0,
            svm_port: 0,
            svm_cid: 0,
            svm_zero: [0; 4],
        };
        let mut len = std::mem::size_of::<SockaddrVm>() as SockLen;
        if getpeername(fd, &mut addr, &mut len) != 0 {
            return None;
        }
        if addr.svm_family as i32 != AF_VSOCK {
            return None;
        }
        Some(addr.svm_cid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_defaults_to_host_only() {
        assert_eq!(parse_allowlist_from(None), vec![VMADDR_CID_HOST]);
    }

    #[test]
    fn allowlist_empty_means_any() {
        assert!(parse_allowlist_from(Some("")).is_empty());
        assert!(parse_allowlist_from(Some("   ")).is_empty());
    }

    #[test]
    fn allowlist_parses_csv() {
        assert_eq!(parse_allowlist_from(Some(" 2, 5,7 ")), vec![2, 5, 7]);
        assert_eq!(parse_allowlist_from(Some("bogus")), vec![VMADDR_CID_HOST]);
    }
}
