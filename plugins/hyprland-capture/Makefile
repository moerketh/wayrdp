# wayrdp-capture Hyprland plugin
#
# Built against the running Hyprland on the guest (0.56.2 headers ship with
# the package in /usr/include/hyprland). Compile *on the guest* because
# Hyprland ships no stable C++ ABI — we must match its compiler.
#
# Hooks Hyprland's per-monitor RENDER_LAST_MOMENT on wayrdp-* headless
# outputs and glReadPixels the painted framebuffer into a tmpfs segment
# ($XDG_RUNTIME_DIR/wayrdp-capture-<name>, seqlock header + BGRA pixels)
# that the backend-wlroots shm pump (WAYRDP_CAPTURE=shm-plugin) consumes.
# The runtime dir is 0700 user-owned, so no other local user can pre-create
# the segment name.
#
# WARNING: never hot-reload (hyprctl plugin unload/load) while wayrdp has
# peers — pluginExit teardown races an in-flight render frame and SIGSEGVs
# the compositor into --safe-mode --locked (observed 2026-10-10). Upgrade
# the .so at logout/reboot. Sessions only use this path with
# WAYRDP_CAPTURE=shm-plugin; without that, wayrdp uses ext-image-copy.

CXX      ?= g++
CXX_SRCS := main.cpp

OUT      := wayrdp-capture.so

# Hyprland's own cflags; hyprland.pc pulls in all transitive deps (drm, egl,
# hyprutils, aquamarine, etc.)
PKG      := hyprland

CXXFLAGS ?= -O2 -fPIC -fno-strict-aliasing -std=c++23

all: $(OUT)

$(OUT): $(CXX_SRCS)
	$(CXX) -shared $(CXXFLAGS) $(CXX_SRCS) -o $(OUT) \
	    $(shell pkg-config --cflags --libs $(PKG))

clean:
	rm -f $(OUT)

.PHONY: all clean
