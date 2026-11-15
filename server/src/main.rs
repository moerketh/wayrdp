//! wayrdp-server entry point.
//!
//! Transports:
//! * AF_VSOCK (default on) — vmconnect Enhanced Session. The Hyper-V host
//!   proxy plus the CID allowlist are the trust boundary, so this channel
//!   needs no further credentials.
//! * TCP (opt-in via `WAYRDP_TCP=1`) — mstsc/xfreerdp/debugging. Fail
//!   closed: TCP peers must authenticate via NLA against a provisioned SAM
//!   file, and the listener binds loopback unless `WAYRDP_BIND` explicitly
//!   widens it. The pre-hardening code served an unauthenticated session on
//!   0.0.0.0:3389; that must never happen implicitly again.
//!
//! Backend selection: `WAYRDP_BACKEND` env — `wlroots` (Hyprland), `kde`
//! (Plasma 6.3+) or `synthetic`. Unset = auto-detect from
//! XDG_CURRENT_DESKTOP (F12): Hyprland→wlroots, KDE→kde; Sway is detected
//! but has no working backend (wlroots drives Hyprland only), so startup
//! fails there with a clear error rather than showing a vmconnect user the
//! synthetic test pattern. `WAYRDP_BACKEND=synthetic` is the escape hatch.
//! These configuration errors exit with status 42 so systemd's
//! `RestartPreventExitStatus=42` stops the retry loop.

use std::path::{Path, PathBuf};
use std::process::Command;

mod selected_backend;
use selected_backend::TaggedError;

fn main() -> anyhow::Result<()> {
    init_tracing();
    let tcp = tcp_enabled();
    let vsock = !matches!(
        std::env::var("WAYRDP_VSOCK").as_deref(),
        Ok("0") | Ok("false") | Ok("no")
    );
    anyhow::ensure!(
        tcp || vsock,
        "no transport enabled: vsock is on by default (disable with WAYRDP_VSOCK=0); \
         TCP needs WAYRDP_TCP=1"
    );

    let dir = data_dir()?;
    let (cert_path, key_path) = generate_self_signed_cert(&dir)?;

    // NLA credentials for TCP. Fail closed when TCP is on and nothing is
    // provisioned; vsock-only runs may proceed without credentials.
    let sam_path = match provision_sam(&dir) {
        Ok(p) => Some(p),
        Err(e) if tcp => return Err(e),
        Err(e) => {
            eprintln!("[wayrdp] no NLA credentials: {e:#}");
            eprintln!(
                "[wayrdp] continuing vsock-only; provision them and set WAYRDP_TCP=1 to enable TCP"
            );
            None
        }
    };

    let backend: Box<dyn desk_backend::DesktopBackend> = match selected_backend() {
        Ok(Backend::Wlroots) => Box::new(backend_wlroots::WlrBackend),
        Ok(Backend::Kde) => {
            use backend_kde::preflight::{preflight, Failure};
            if let Err(f) = preflight() {
                eprintln!("[wayrdp] kde backend: {f}");
                std::process::exit(match f {
                    Failure::Permanent(_) => selected_backend::EXIT_NO_BACKEND,
                    Failure::Transient(_) => 1,
                });
            }
            // Restore physical outputs a crashed previous run left disabled.
            backend_kde::recovery::recover_at_startup();
            Box::new(backend_kde::KdeBackend)
        }
        Ok(Backend::Synthetic) => Box::new(rdp_core::synthetic::SyntheticBackend),
        Err(e) => {
            eprintln!("[wayrdp] {e:#}");
            std::process::exit(selected_backend::exit_code(&e));
        }
    };
    let port: u16 = std::env::var("WAYRDP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3389);
    let vsock_allow = rdp_core::vsock::parse_allowlist();
    let bind: std::net::SocketAddr = (tcp_bind_ip()?, port).into();
    let server = rdp_core::Server {
        backend,
        bind,
        cert_path: cert_path.to_string_lossy().into_owned(),
        key_path: key_path.to_string_lossy().into_owned(),
        vsock,
        vsock_allow: vsock_allow.clone(),
        tcp,
        sam_path: sam_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
    };
    eprintln!(
        "wayrdp: vsock {} allow {:?}; cert {} key {}",
        if vsock { "on" } else { "off" },
        vsock_allow,
        cert_path.display(),
        key_path.display(),
    );
    if tcp {
        eprintln!(
            "wayrdp: tcp on {bind} (NLA only, SAM {})",
            sam_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<none>".into())
        );
    }
    server.run()
}

/// TCP serving is opt-in: unlike the vsock channel it has no host-proxy
/// trust boundary, so it must never come up unconfigured.
fn tcp_enabled() -> bool {
    matches!(
        std::env::var("WAYRDP_TCP").as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

/// TCP bind address: loopback unless `WAYRDP_BIND` explicitly widens it.
fn tcp_bind_ip() -> anyhow::Result<std::net::IpAddr> {
    match std::env::var("WAYRDP_BIND") {
        Ok(raw) => raw
            .parse()
            .map_err(|e| anyhow::anyhow!("WAYRDP_BIND={raw:?} is not a valid IP address: {e}")),
        Err(_) => Ok(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
    }
}

#[derive(Debug)]
enum Backend {
    Wlroots,
    Kde,
    Synthetic,
}

/// Backend selection (F12): explicit `WAYRDP_BACKEND` wins; otherwise detect
/// the compositor from XDG_CURRENT_DESKTOP so a stock install does the right
/// thing. Auto-detection only maps compositors a backend can actually serve:
/// Sway is a startup error, not a silent test pattern — the wlroots backend
/// drives Hyprland (`hyprctl`), so serving `synthetic` there would show a
/// vmconnect user the test pattern instead of telling them nothing works.
/// `WAYRDP_BACKEND=synthetic` is the explicit escape hatch. Explicitly asking
/// for a backend that doesn't exist (a typo) is a startup error, not a
/// per-connection surprise or a silent test pattern.
fn selected_backend() -> anyhow::Result<Backend> {
    let explicit = std::env::var("WAYRDP_BACKEND")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(v) = explicit {
        return explicit_backend(&v);
    }
    let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let backend = detected_backend(&desktops)?;
    if let Backend::Synthetic = backend {
        eprintln!(
            "[wayrdp] no supported compositor in XDG_CURRENT_DESKTOP={desktops:?}; \
             serving the synthetic test backend (set WAYRDP_BACKEND to override)"
        );
    } else {
        eprintln!("[wayrdp] backend auto-selected: {backend:?} (XDG_CURRENT_DESKTOP={desktops})");
    }
    Ok(backend)
}

/// Map an XDG_CURRENT_DESKTOP value (colon-separated) to a backend.
fn detected_backend(desktops: &str) -> anyhow::Result<Backend> {
    for tag in desktops.split(':').map(str::trim) {
        match tag.to_ascii_lowercase().as_str() {
            "hyprland" => return Ok(Backend::Wlroots),
            "kde" | "plasma" => return Ok(Backend::Kde),
            "sway" => {
                let e = anyhow::anyhow!(
                    "XDG_CURRENT_DESKTOP={desktops} has no working wayrdp backend \
                     (the wlroots backend drives Hyprland only) — refusing to \
                     silently serve the synthetic test pattern. Set \
                     WAYRDP_BACKEND=synthetic to force the test backend."
                );
                return Err(selected_backend::EXIT_NO_BACKEND.apply(e));
            }
            _ => {}
        }
    }
    Ok(Backend::Synthetic)
}

/// stderr sink for `tracing` that swallows write errors. A full /tmp (the
/// unit's stderr can be a file there) must not turn a log line into a panic
/// inside a FreeRDP callback: see the 2026-10-13 stderr-panic note.
struct QuietStderr;

impl std::io::Write for QuietStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stderr().flush();
        Ok(())
    }
}

/// Route library `tracing` events (hyperv-rdp-extras) to stderr, filtered by
/// `RUST_LOG` (default `info`). No timestamps or ANSI: the journal adds the
/// former, and escape codes break log greps.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(|| QuietStderr)
        .with_ansi(false)
        .without_time()
        .try_init();
}

/// Resolve a non-empty `WAYRDP_BACKEND` value.
fn explicit_backend(value: &str) -> anyhow::Result<Backend> {
    match value {
        "wlroots" => Ok(Backend::Wlroots),
        "kde" => Ok(Backend::Kde),
        "synthetic" => Ok(Backend::Synthetic),
        other => Err(selected_backend::EXIT_NO_BACKEND.apply(anyhow::anyhow!(
            "unknown WAYRDP_BACKEND={other:?}; use wlroots, kde or synthetic"
        ))),
    }
}

/// `$XDG_DATA_HOME/wayrdp` (or `~/.local/share/wayrdp`): TLS cert/key and
/// NLA credentials. Hard error when neither variable is set — falling back
/// to a shared world-writable tempdir would let any local user steal or
/// swap the TLS key and SAM file (predictable-path attack).
fn data_dir() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "neither XDG_DATA_HOME nor HOME is set; \
                 refusing to store credentials under a shared tempdir"
            )
        })?;
    let dir = base.join("wayrdp");
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The 0700 mode protects everything created below (cert, key, SAM).
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Generate (or reuse) a persistent self-signed cert via the openssl CLI.
///
/// The cert is reused across service restarts, so an RDP client's
/// certificate trust (or pinned fingerprint) remains valid instead of being
/// invalidated by a fresh ephemeral cert every boot. Reusing a stable cert
/// is what lets first-time "accept certificate" actually stick. We use the
/// CLI instead of pulling rustls/openssl-rs into the dep tree, so the
/// dependency surface stays minimal.
fn generate_self_signed_cert(dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");

    // Reuse only when both files exist AND the cert still parses and is not
    // about to expire; a corrupt or near-expired cert is regenerated so the
    // service never wedges on stale material.
    if cert.exists() && key.exists() && cert_ok(&cert) {
        return Ok((cert, key));
    }

    let status = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "365",
            "-subj",
            "/CN=wayrdp.test",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .status()?;
    anyhow::ensure!(status.success(), "openssl req failed");
    // The key must not be world-readable, or TLS libs may reject it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600));
    }
    Ok((cert, key))
}

/// Parse + expiry check via the openssl CLI: the cert must be a valid X.509
/// document AND still have more than seven days to live.
fn cert_ok(cert: &Path) -> bool {
    let parses = Command::new("openssl")
        .args(["x509", "-noout", "-in"])
        .arg(cert)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let fresh = Command::new("openssl")
        .args(["x509", "-checkend", "604800", "-noout", "-in"])
        .arg(cert)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    parses && fresh
}

/// Provision the NLA credential store (FreeRDP `NtlmSamFile`) at
/// `<data_dir>/SAM`.
///
/// The credentials exist to keep out everyone who does not know the
/// password — they are not an interactive UX. An interactive
/// prompt-at-first-start would leave the daemon hung on stdin forever under
/// systemd, and default/auto-generated passwords are indistinguishable from
/// no auth from an attacker's point of view, so instead we fail closed and
/// print the exact provisioning command.
///
/// Format (winpr SAM): `User:Domain:<LM hash>:<NT hash>:` per line; the
/// NT hash is hex MD4 of the password encoded as UTF-16LE. MD4 is gone
/// from OpenSSL 3's default provider — which also breaks python3's
/// `hashlib.new("md4")` — so the printed provisioning command uses
/// `winpr-hash3` (ships with FreeRDP), which writes the hash into the
/// NT field (`wayrdp:::<nthash>:::`). Returns the SAM path when at least
/// one usable credential entry exists.
fn provision_sam(dir: &Path) -> anyhow::Result<PathBuf> {
    let sam = dir.join("SAM");

    let usable = std::fs::read_to_string(&sam)
        .ok()
        .filter(|text| text.lines().any(|l| sam_line_usable(l)))
        .is_some();
    if usable {
        return Ok(sam);
    }

    anyhow::bail!(
        "no usable NLA credentials in {sam}.\n\
         Provision a password once and restart:\n  \
         read -s pw && winpr-hash3 -u wayrdp -p \"$pw\" -f sam > {sam} \
         && unset pw && chmod 600 {sam}\n\
         (winpr-hash3 ships with FreeRDP; read -s keeps the password out \
         of shell history)",
        sam = sam.display()
    );
}

/// A SAM line is usable when it is `name:domain:lm:nthash:`-shaped with a
/// 32-hex NT-hash field. Malformed or comment lines do not count.
fn sam_line_usable(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return false;
    }
    let f: Vec<&str> = line.split(':').collect();
    f.len() >= 4
        && !f[0].is_empty()
        && f[3].len() == 32
        && f[3].bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_backend_values_resolve() {
        assert!(matches!(explicit_backend("wlroots"), Ok(Backend::Wlroots)));
        assert!(matches!(explicit_backend("kde"), Ok(Backend::Kde)));
        assert!(matches!(
            explicit_backend("synthetic"),
            Ok(Backend::Synthetic)
        ));
    }

    #[test]
    fn desktop_detection() {
        assert!(matches!(detected_backend("Hyprland"), Ok(Backend::Wlroots)));
        assert!(matches!(detected_backend("KDE"), Ok(Backend::Kde)));
        assert!(matches!(detected_backend("ubuntu:KDE"), Ok(Backend::Kde)));
        assert!(matches!(detected_backend("plasma"), Ok(Backend::Kde)));
        assert!(matches!(detected_backend("GNOME"), Ok(Backend::Synthetic)));
        assert!(matches!(detected_backend(""), Ok(Backend::Synthetic)));
        let Err(e) = detected_backend("sway") else {
            panic!("sway must not select a backend");
        };
        assert_eq!(
            selected_backend::exit_code(&e),
            selected_backend::EXIT_NO_BACKEND
        );
    }

    #[test]
    fn typos_exit_with_no_backend_status() {
        for value in ["KDE", "wlroot", "Wlroots", "plasma"] {
            let Err(e) = explicit_backend(value) else {
                panic!("{value} must not select a backend");
            };
            assert_eq!(
                selected_backend::exit_code(&e),
                selected_backend::EXIT_NO_BACKEND,
                "{value}"
            );
        }
    }
}
