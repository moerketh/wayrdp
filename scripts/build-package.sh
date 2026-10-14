#!/usr/bin/env bash
# Build a release package for wayrdp. Used directly for local builds and by
# the tag-triggered CI release pipeline (.github/workflows/release.yml).
#
# Produces under dist/release/:
#   wayrdp-<ver>-linux-x86_64.tar.gz   portable bundle (bin/wayrdp + systemd
#                                      user unit + install.sh, /usr/local prefix)
#                                      — the universal artifact; works on Arch
#                                      (no makepkg needed) AND Debian-family.
#   wayrdp_<ver>_amd64.deb             Debian/Ubuntu package (if dpkg-deb present)
#   SHA256SUMS.txt                     checksums for the artifacts above
#
# wayrdp is a single server binary + one systemd USER unit; there is no GUI,
# D-Bus, polkit, PAM, or codec license set, so this script intentionally
# stays minimal.
#
# Run from a WSL/Linux checkout:
#   scripts/build-package.sh [--version 0.1.0] [--skip-build] [--skip-deb]
#
# Exit codes: 0 success; 1 usage; 2 missing dependency; 3 build failure;
# 4 packaging failure.

set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$PWD"

WAYRDP_VERSION="${WAYRDP_VERSION:-}"
ARCH="$(uname -m)"
DIST_DIR="${REPO_ROOT}/dist/release"

# Stage on a NATIVE filesystem, never the repo path: under WSL /mnt/c (drvfs)
# every file reads as mode 777, which dpkg-deb rejects for DEBIAN/ and which
# would leak 777 modes into the tarball entries. mktemp -d gives /tmp (ext4)
# under WSL.
STAGE_ROOT="$(mktemp -d)"
trap 'rm -rf "$STAGE_ROOT"' EXIT

usage() {
  cat <<EOF
Usage: $0 [--version <ver>] [--skip-build] [--skip-deb]
  --version <ver>   Package version (default: server crate version)
  --skip-build      Reuse an existing target/release build
  --skip-deb        Do not produce the .deb
  -h|--help         Show this help
Environment:
  WAYRDP_VERSION    Same as --version
  CARGO_TARGET_DIR  Cargo target dir (default: <repo>/target)
EOF
}

SKIP_BUILD=0
SKIP_DEB=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) [[ $# -ge 2 ]] || { echo "error: --version needs a value" >&2; exit 1; }
      WAYRDP_VERSION="$2"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --skip-deb) SKIP_DEB=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

log() { printf '\033[1;36m[wayrdp-package]\033[0m %s\n' "$*"; }
err() { printf '\033[1;31m[wayrdp-package:ERROR]\033[0m %s\n' "$*" >&2; }
die() { err "$2"; exit "$1"; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die 2 "missing dependency: $1 (install it and re-run)"
}

# Version: cargo metadata of the server crate (package `wayrdp`).
if [[ -z "$WAYRDP_VERSION" ]]; then
  WAYRDP_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' server/Cargo.toml | head -1)"
fi
[[ -n "$WAYRDP_VERSION" ]] || die 1 "could not determine version; pass --version"
log "version: $WAYRDP_VERSION  arch: $ARCH"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
export CARGO_TARGET_DIR

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
build() {
  [[ "$SKIP_BUILD" = "1" ]] && { log "skip-build: reusing $CARGO_TARGET_DIR"; return; }
  require_cmd cargo
  export RUSTUP_TOOLCHAIN=stable
  # rust-lld fails to link some native libs; force the system cc linker.
  export RUSTFLAGS="-C linker=cc ${RUSTFLAGS:-}"
  export CARGO_PROFILE_RELEASE_LTO=thin
  export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=4
  cargo build --release -p wayrdp || die 3 "cargo build failed"
}
build
BIN="$CARGO_TARGET_DIR/release/wayrdp"
[[ -f "$BIN" ]] || die 3 "wayrdp binary missing after build ($BIN)"
log "build OK: $BIN"

# ---------------------------------------------------------------------------
# Shared install-set staging (binary + user unit + license)
# ---------------------------------------------------------------------------
stage_install_set() {  # $1 = destination root that receives usr/...
  local dest="$1"
  install -Dm755 "$BIN" "$dest/usr/bin/wayrdp"
  install -Dm644 "$REPO_ROOT/packaging/systemd/wayrdp.service" \
      "$dest/usr/lib/systemd/user/wayrdp.service"
  install -Dm644 "$REPO_ROOT/LICENSE" "$dest/usr/share/licenses/wayrdp/LICENSE"
  # Hyprland shm-capture plugin, SOURCE ONLY: Hyprland ships no stable C++
  # ABI, so the .so must be built on the guest against its own headers
  # (installers detect Hyprland + g++/headers and build or skip).
  install -Dm644 "$REPO_ROOT/plugins/hyprland-capture/main.cpp" \
      "$dest/usr/share/wayrdp/plugins/hyprland-capture/main.cpp"
  install -Dm644 "$REPO_ROOT/plugins/hyprland-capture/Makefile" \
      "$dest/usr/share/wayrdp/plugins/hyprland-capture/Makefile"
}

# ---------------------------------------------------------------------------
# Portable tarball (bin/ + lib/ + share/ + install.sh; /usr/local prefix)
# ---------------------------------------------------------------------------
build_tarball() {
  require_cmd tar
  local tar_root="wayrdp-${WAYRDP_VERSION}-linux-${ARCH}"
  local out="${DIST_DIR}/wayrdp-${WAYRDP_VERSION}-linux-${ARCH}.tar.gz"
  local stage="${STAGE_ROOT}/tarball"
  rm -rf "$stage"
  mkdir -p "$stage/$tar_root"

  install -Dm755 "$BIN" "$stage/$tar_root/bin/wayrdp"
  install -Dm644 "$REPO_ROOT/packaging/systemd/wayrdp.service" \
      "$stage/$tar_root/lib/systemd/user/wayrdp.service"
  install -Dm644 "$REPO_ROOT/LICENSE" "$stage/$tar_root/share/doc/wayrdp/LICENSE"
  install -Dm644 "$REPO_ROOT/plugins/hyprland-capture/main.cpp" \
      "$stage/$tar_root/share/wayrdp/plugins/hyprland-capture/main.cpp"
  install -Dm644 "$REPO_ROOT/plugins/hyprland-capture/Makefile" \
      "$stage/$tar_root/share/wayrdp/plugins/hyprland-capture/Makefile"

  cat > "$stage/$tar_root/install.sh" <<'EOF'
#!/usr/bin/env bash
# Install wayrdp from the portable tarball.
# Default prefix: /usr/local (systemd user unit under <prefix>/lib/systemd/user)
set -euo pipefail
if [[ $EUID -ne 0 ]]; then
  echo "run as root: sudo ./install.sh [--prefix /usr/local]" >&2
  exit 1
fi
PREFIX="/usr/local"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done
SRC="$(cd "$(dirname "$0")" && pwd)"
install -Dm755 "$SRC/bin/wayrdp" "$PREFIX/bin/wayrdp"
# The shipped unit hardcodes ExecStart=/usr/bin/wayrdp (the deb path). For the
# portable tarball the binary actually lands at $PREFIX/bin (default /usr/local),
# so rewrite ExecStart to the real install location — otherwise systemd fails
# with status=203/EXEC and the service auto-restart-loops without ever listening.
TMP_UNIT="$(mktemp)"
sed -E "s#^ExecStart=.*#ExecStart=$PREFIX/bin/wayrdp#" \
    "$SRC/lib/systemd/user/wayrdp.service" > "$TMP_UNIT"
install -Dm644 "$TMP_UNIT" "$PREFIX/lib/systemd/user/wayrdp.service"
rm -f "$TMP_UNIT"
install -Dm644 "$SRC/share/doc/wayrdp/LICENSE" \
    "$PREFIX/share/doc/wayrdp/LICENSE"
if [[ -d "$SRC/share/wayrdp/plugins" ]]; then
    # mkdir first: with $PREFIX/share/wayrdp absent, cp -r would create the
    # 'plugins' dir AS 'wayrdp' and drop the plugins/ level.
    mkdir -p "$PREFIX/share/wayrdp"
    cp -r "$SRC/share/wayrdp/plugins" "$PREFIX/share/wayrdp/"
fi
echo "wayrdp installed to $PREFIX"
echo "Enable for the desktop user (needs a live Wayland session):"
echo "  systemctl --user daemon-reload"
echo "  systemctl --user enable --now wayrdp.service"
echo "Survive logout (RDP before local login): sudo loginctl enable-linger <user>"
EOF
  chmod 755 "$stage/$tar_root/install.sh"

  mkdir -p "$DIST_DIR"
  ( cd "$stage" && tar -czf "$out" "$tar_root" ) || die 4 "tarball build failed"
  log "built $(basename "$out")"
}

# ---------------------------------------------------------------------------
# .deb (dpkg-deb staging)
# ---------------------------------------------------------------------------
build_deb() {
  [[ "$SKIP_DEB" = "1" ]] && { log "skip-deb"; return; }
  command -v dpkg-deb >/dev/null 2>&1 || { log "dpkg-deb not present — skipping .deb"; return; }
  local stage="${STAGE_ROOT}/deb"
  local out="${DIST_DIR}/wayrdp_${WAYRDP_VERSION}_amd64.deb"
  rm -rf "$stage"
  mkdir -p "$stage/DEBIAN"
  stage_install_set "$stage"

  cat > "$stage/DEBIAN/control" <<EOF
Package: wayrdp
Version: ${WAYRDP_VERSION}
Section: net
Priority: optional
Architecture: amd64
Maintainer: moerketh <https://github.com/moerketh>
Depends: libc6, libwayland-client0, libxkbcommon0, openssl, libfreerdp-server3-3, libfreerdp3-3, libwinpr3-3
Description: Wayland-native RDP server on FreeRDP 3
 wayrdp is a Wayland-native RDP server built on the FreeRDP 3
 (libfreerdp-server) core with compositor-specific capture/input backends:
 a wlroots backend (Hyprland headless output + ext-image-copy-capture and
 Sway) and a KDE KWin virtual-output backend, selected at runtime via
 WAYRDP_BACKEND. Runs as a systemd user unit inside the live Wayland
 session.
EOF

  cat > "$stage/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
echo "wayrdp installed."
echo "Enable for the desktop user (needs a live Wayland session):"
echo "  systemctl --user daemon-reload"
echo "  systemctl --user enable --now wayrdp.service"
echo "Survive logout: sudo loginctl enable-linger <user>"
exit 0
EOF
  cat > "$stage/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
exit 0
EOF
  chmod 755 "$stage/DEBIAN/postinst" "$stage/DEBIAN/postrm"

  mkdir -p "$DIST_DIR"
  dpkg-deb --build --root-owner-group "$stage" "$out" || die 4 "dpkg-deb build failed"
  rm -rf "$stage"
  log "built $(basename "$out")"
}

build_tarball
build_deb

# ---------------------------------------------------------------------------
# Checksums
# ---------------------------------------------------------------------------
if command -v sha256sum >/dev/null 2>&1; then
  ( cd "$DIST_DIR" && sha256sum ./*.tar.gz ./*.deb > SHA256SUMS.txt 2>/dev/null ) || true
  log "wrote SHA256SUMS.txt"
fi

log "done -> $DIST_DIR"
ls -la "$DIST_DIR"
