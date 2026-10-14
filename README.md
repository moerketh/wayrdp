# wayrdp

Wayland-native RDP server for the Hyper-V enhanced-session style of use:
attach to an existing graphical session, dynamic virtual outputs,
EGFX RemoteFX streaming — compositor-aware backends behind one trait.

MIT licensed, built on FreeRDP 3 (`libfreerdp-server3`, Apache-2.0).

## Status

**Working today (Hyprland guests):**

* AF_VSOCK (Enhanced Session / vmconnect) listener with CID allowlist, on
  by default; TCP opt-in behind NLA (SAM-provisioned, loopback by
  default); persistent self-signed TLS so client trust survives restarts.
* Full enhanced-session path: headless virtual output at the client's
  size, Display-Control live resize, RemoteFX-over-EGFX frames (tiling
  diffs opt-in via `WAYRDP_TILED_DIFF`), client-layout-matched xkb
  keymap, client desktop-scale mirroring, virtual pointer/keyboard input.
* Stability hardened against real vmconnect churn: peer teardown
  ordering, client-EOF and pre-activation watchdogs, EGFX back-pressure
  that survives clients which suspend frame acks, timed `hyprctl` calls,
  panic-proofed FFI paths, Hyprland 0.56 keyboard-churn avoidance.
* A self-asserting vmconnect regression harness (host-side prober plus
  gates over SSH) covering connection, activation, capture liveness,
  frame flow, frame content, compositor health, restart churn and
  mid-activation teardown.

**Stub / pending:**

* `backend-kde` returns `Unsupported` pending `hyperv-rdp-extras`
  `kwin-virtual` wiring (extras stays an external shared library;
  nothing is forked into this repo).
* Planned further backends: `backend-portal` (generic
  xdg-desktop-portal fallback), GNOME — all behind the same
  `desk-backend` trait.

## Layout

| Path | Purpose |
|---|---|
| `crates/freerdp-sys` | bindgen + C shim over freerdp-server3 (build-time generated) |
| `crates/desk-backend` | the compositor-agnostic backend trait — zero Wayland deps |
| `crates/rdp-core` | safe-Rust RDP server core (listener, peer, EGFX/RemoteFX, dispctrl, input, vsock) |
| `crates/backend-wlroots` | Hyprland: headless output + capture (ext-image-copy-capture or shm-plugin) + virtual pointer/keyboard |
| `crates/backend-kde` | Plasma virtual-output adapter (stub; ships with hyperv-rdp-extras wiring) |
| `server` | the `wayrdp` binary |
| `plugins/hyprland-capture` | Hyprland C++ capture plugin for the shm path (built on the guest) |
| `tools/` | VMConnect host-side regression harness + prober, compositor stress, hvsocket forwarder |
| `scripts/` | release packaging, vsock send-path tracing |
| `packaging/` | systemd user unit, Arch PKGBUILD |

Unit tests are deliberately few (synthetic frames and vsock lifetimes in
`rdp-core`, backend selection in `server`); real validation is the harness
plus manual client checks (see [Validating](#validating)).

## Run

Backend auto-selects from `XDG_CURRENT_DESKTOP`: Hyprland → `wlroots`.
KDE/Plasma and Sway are startup errors (exit 42 — a restart can't fix
"no backend", so the systemd unit's `RestartPreventExitStatus` stops the
retry loop) rather than silently serving the synthetic test desktop on a
real session. An unrecognized or unset desktop falls back to `synthetic`
with a warning; override explicitly with `WAYRDP_BACKEND=wlroots|synthetic`
(`kde` is still a stub).

On Hyprland, capture runs either over `ext-image-copy-capture-v1`
(default) or via the `hyprland-capture` plugin (`WAYRDP_CAPTURE=shm-plugin`)
which publishes the painted framebuffer plus compositor damage rects into
a per-output seqlock shm segment — much cheaper on llvmpipe guests since
it adds no compositor-side fake render. The plugin is C++ and must be
built **on the guest** against its exact Hyprland headers; it refuses to
load on an API-hash mismatch, and it must not be hot-swapped in-session
(unloading it while Hyprland runs can crash the compositor). See
[`plugins/hyprland-capture/`](plugins/hyprland-capture/).

Transports:

* AF_VSOCK (default on, `WAYRDP_VSOCK=0` to disable) — vmconnect Enhanced
  Session over the VMBus, guarded by the Hyper-V host proxy plus the
  `WAYRDP_VSOCK_ALLOW` CID allowlist; negotiates the legacy RDP security
  layer to match vmconnect's no-encryption vsock mode.
* TCP (opt-in, `WAYRDP_TCP=1`) — loopback by default (`WAYRDP_BIND` to
  widen), NLA-only: TLS-only clients are refused with HYBRID_REQUIRED.
  The server fails closed until credentials are provisioned; it prints the
  exact `winpr-hash3` SAM provisioning command when they are missing.

TLS cert/key persist under `$XDG_DATA_HOME/wayrdp` (0600 key, 0700 dir) so
first-time client trust sticks across restarts.

## Build

Requires FreeRDP 3 dev libraries (`freerdp3-dev` on Debian/Ubuntu/Kali)
providing pkg-config `freerdp-server3 freerdp3 winpr3`, plus clang for
bindgen; builds happen on Linux (WSL works). Without the headers the
workspace still compiles (`freerdp-sys` builds empty and the RDP core is
cfg'd out — the server then refuses to start), so a green `cargo check`
on Windows says nothing about the real server path.

```sh
cargo check --workspace
cargo build --release -p wayrdp          # -> target/release/wayrdp
cargo test --workspace
scripts/build-package.sh                 # -> dist/release/ (tar.gz, .deb, SHA256SUMS)
```

The tar.gz is the portable artifact — `wayrdp` binary + install script +
systemd **user** unit (`/usr/local` prefix, works on Arch and Debian
family); Arch users can instead build from `packaging/arch/PKGBUILD`. The
unit requires a Wayland session (`ConditionEnvironment=WAYLAND_DISPLAY`).

## Validating

* **Harness (primary gate):** from a Windows host, run
  `tools/vmconnect-harness.ps1`. It drives the real vmconnect wire path
  via `tools/vmconnect-probe.cs` (AF_HYPERV → guest 3389 → raw MCS
  Connect-Initial — the exact point where generic clients stall) and
  asserts over SSH: compositor health before the run, connection and
  activation, shm capture liveness, frame flow, non-black frame content,
  no compositor crash or safe-mode, single-peer lifecycle, restart churn
  and mid-activation teardown.
* **vmconnect path for ordinary clients:** `tools/vsfwd.cs` (C#) is a
  TCP↔AF_HYPERV forwarder so xfreerdp3 can be driven through the same
  wire path; `tools/compositor-stress.sh` reproduces the headless-output
  retry storm directly against the compositor.
* **Latency work:** `scripts/vsock-trace.sh` +
  `scripts/vsock-trace-summary.awk` time each syscall on the live AF_VSOCK
  channel; `tools/shm_pixsample.py` samples the capture shm segment for
  content checks.
* **Manual matrix:** mstsc, VMConnect Enhanced Session, or
  `xfreerdp3 /dynamic-resolution`.

## Known limitations & roadmap

* **Codec is RemoteFX only.** H.264/AVC444 surface codecs are planned
  (OpenH264 first — x264/GPL never links in, per the locked licensing
  decisions); RFX text sharpness and gradient fidelity are codec-capped.
* **No client-drawn cursor relay yet.** `WAYRDP_POINTER_METADATA` is
  half-built and off by default — without cursor-shape support it makes
  FreeRDP-family clients warp their local cursor.
* **shm capture is copy-based** (render + read out of the plugin's
  segment). Zero-copy dmabuf export is a tracked fork-scale item.
* **mstsc live resize is unconfirmed** — Display-Control resize is
  verified with xfreerdp3 `/dynamic-resolution`; mstsc honors the
  selected resolution on reconnect.
* **Hyprland 0.56.x caveats:** virtual-keyboard churn around `hyprctl
  devices` IPC can SEGV upstream compositors; avoid that query during
  connect/disconnect storms, and never run long-lived sessions with
  `WLOG_LEVEL=TRACE` (WinPR's BinaryAppender fills the tmpfs).
