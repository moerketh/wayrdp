// wayrdp-capture: Hyprland plugin that hands frames directly to wayrdp.
//
// skipping the ext-image-copy protocol's beginFullFakeRender dance.
//
// On Hyprland 0.56.2 + wlroots software renderer, ext-image-copy's
// copyShm() invokes beginFullFakeRender → render() → readPixels, which
// re-renders the whole monitor into a fresh FBO on every request.
// At 1920x1200 llvmpipe that's ~40ms per request, capping fps at ~25.
//
// This plugin instead hooks Hyprland's own per-monitor render path at
// RENDER_LAST_MOMENT (arm) and CHyprGLRenderer::endRender (GL context still
// bound, the frame's pass executed — see e9). We glReadPixels from the
// painted output FBO into a memfd, then notify via a Unix
// datagram socket. wayrdp consumes the shm.
//
// Cost per frame: one FB readPixels (~1-3ms) vs 40ms for fake render.

#include <hyprland/src/Compositor.hpp>
#include <hyprland/src/debug/log/Logger.hpp>
#include <hyprland/src/event/EventBus.hpp>
#include <hyprland/src/helpers/Color.hpp>
#include <hyprland/src/helpers/memory/Memory.hpp>
#include <hyprland/src/output/DamageRing.hpp>
#include <hyprland/src/plugins/HookSystem.hpp>
#include <hyprland/src/plugins/PluginAPI.hpp>
#include <hyprland/src/pointer/PointerManager.hpp>
#include <hyprland/src/render/OpenGL.hpp>
#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/render/gl/GLFramebuffer.hpp>
#include <hyprland/src/version.h>

#include <hyprutils/math/Region.hpp>
#include <hyprutils/signal/Signal.hpp>
#include <hyprutils/signal/Listener.hpp>
#include <hyprutils/memory/WeakPtr.hpp>

#include <GLES3/gl32.h>
#include <drm_fourcc.h>

#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <fcntl.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cmath>
#include <cstdint>
#include <functional>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <stdexcept>
#include <string>
#include <unordered_map>

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

inline HANDLE g_pHandle = nullptr;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

static Hyprutils::Signal::CHyprSignalListener g_onRenderStage;
static Hyprutils::Signal::CHyprSignalListener g_onRenderPre;
static Hyprutils::Signal::CHyprSignalListener g_onMonitorDestroy;
static Hyprutils::Signal::CHyprSignalListener g_onCursorChanged;

static std::string  g_targetPrefix = "wayrdp";
static bool         g_inWayrdpMon  = false;

// How long a Target sits out after a failed segment setup (full runtime
// dir): the render path retries at most once per window instead of once
// per frame, so a full tmpfs is not hammered ~60x/s from inside Hyprland.
static constexpr uint64_t SETUP_RETRY_BACKOFF_MS = 3000;
static PHLMONITOR   g_currentMon   = nullptr;

// Per-monitor capture target. Paths are keyed by monitor name: two
// concurrent wayrdp sessions create distinct wayrdp-N outputs and must not
// race on one shm (observed live: a second 1366x768 peer rendered into the
// 1920x1200 buffer => black screen; also a reconnect allocating a fresh
// wayrdp-N name must not strand the producer on the old paths).
struct Target {
    int      shmFd   = -1;
    void*    shmPtr  = nullptr;
    size_t   shmSize = 0;
    uint32_t frameW  = 0;
    uint32_t frameH  = 0;
    uint64_t seq     = 0;
    uint64_t dropNoPeer = 0;
    // e8: which bank the NEXT frame goes to, and the last seq published per
    // bank (0 = never published → no wait needed). A bank may be reused
    // once releasedSeq[i] >= publishedSeq[i].
    uint32_t writeSlot = 0;
    uint64_t publishedSeq[2] = {0, 0};
    uint64_t dropSlotBusy  = 0; // stats: reader held a bank past 8ms budget
    // Client-drawn cursor as last seen in the header; a change repaints the
    // whole output so no stale painted cursor stays behind in the frame.
    bool     clientCursor = false;
    // Cursor image shm (client-drawn cursor), independent of the frame
    // geometry: it lives until the monitor goes away.
    int      curFd  = -1;
    void*    curPtr = nullptr;
    uint64_t curSeq = 0;
    char     shmPath[256];
    char     curPath[256];
    char     sockPath[256];
    // Backoff after a failed segment setup (full runtime dir etc.):
    // captureFrame runs ~60x/s inside Hyprland's render thread, and each
    // retry would re-create the file and re-run posix_fallocate for up to
    // tens of MB of tmpfs pages before failing — refilling the runtime dir
    // to its cap and starving everything else under /run/user/$UID. After
    // a failure we skip setup for this monitor until the deadline passes
    // or the geometry changes. retryW/H remember the geometry the failure
    // belongs to so a size change retries immediately.
    uint64_t setupRetryAtMs = 0;
    uint32_t retryW = 0, retryH = 0;

    Target(const std::string& monName) {
        // Segments live under XDG_RUNTIME_DIR (tmpfs, 0700, user-owned):
        // same shared-memory semantics as /dev/shm, but a different local
        // user cannot pre-create the name — the /dev/shm squat attack
        // (predictable name + ownership-gated takeover refusal = capture
        // denial) has no purchase here. Without a runtime dir there is no
        // safe location; capture for this monitor stays off (logged where
        // shmCreateOwned fails). The reader (wayrdp backend-wlroots)
        // derives the identical paths.
        const char* rt = std::getenv("XDG_RUNTIME_DIR");
        if (rt && *rt) {
            std::snprintf(shmPath, sizeof(shmPath),
                          "%s/wayrdp-capture-%s", rt, monName.c_str());
            std::snprintf(curPath, sizeof(curPath),
                          "%s/wayrdp-cursor-%s", rt, monName.c_str());
        } else {
            shmPath[0] = '\0';
            curPath[0] = '\0';
            sockPath[0] = '\0';
        }
        // Frame-notify datagram target (best-effort; nothing binds it yet).
        // Same user-private runtime dir — no /tmp fallback: an empty
        // sockPath simply disables the notify fast-path (sockNotify's
        // sendto fails and the reader polls), which is the same rule the
        // segments follow.
        if (rt && *rt) {
            std::snprintf(sockPath, sizeof(sockPath),
                          "%s/wayrdp-capture-%s.sock", rt, monName.c_str());
        }
    }
    Target(const Target&) = delete;
    Target& operator=(const Target&) = delete;
};

static std::unordered_map<std::string, Target> g_targets;

static bool g_cursorHooked = false; // both client-cursor hooks are live

static int          g_sockFd      = -1;
static uint64_t     g_frameSeq    = 0;

// One damage rectangle, top-left origin, pixels.
struct FrameRect {
    uint32_t x, y, w, h;
};

// Header layout version written into FrameHeader::version. 0/absent =
// legacy 24-byte header (pre-damage builds); 1 = the extended 160-byte
// header below with up to MAX_DAMAGE_RECTS compositor damage rects;
// 2 = v1 fields verbatim + e8 double-buffer handshake (Tail below) with
// TWO pixel banks at HEADER_SIZE/HEADER_SIZE+px.
static const uint32_t HDR_VERSION = 2;
static const size_t   MAX_DAMAGE_RECTS = 8; // Hyprland's own damage cap

// Frame header layout in shm (then pixels).
// seq semantics: odd = write in progress, even = stable frame (seqlock).
//
// Versioned extension: legacy plugins ended the header at seq (24 bytes)
// and put pixels right after. v1 adds version/rect_count/rects; readers
// detect it via version == 1 AND a file-size check (a legacy file is too
// small for the extended header + pixels, so pixel bytes can never be
// misread as a version), then read rects with per-rect bounds sanity. If
// anything is off, readers treat the frame as full-surface — never UB.
struct FrameHeader {
    uint32_t magic;      // 0x57524343 "WCRC"
    uint32_t width;
    uint32_t height;
    uint32_t pad;        // CLOCK_MONOTONIC millis at publish, mod 2^32; 0 =
                         // unstamped (old plugin). Reader diffs to measure
                         // capture→consume queue time. Wraps every ~49.7d;
                         // wrapping delta math makes that harmless.
    uint64_t seq;        // increments every frame write
    // --- v1 extension (e2a) ---
    uint32_t version;    // HDR_VERSION; written with the pixels each frame
    uint32_t rect_count; // valid rects this frame; 0 = trust nothing, the
                         // frame is full-surface (empty damage at
                         // RENDER_LAST_MOMENT is ambiguous, not "no change")
    FrameRect rects[MAX_DAMAGE_RECTS]; // compositor damage, clipped to the
                                       // frame; top-left origin to match
                                       // the glReadPixels output rows
};

static uint64_t nowMonoMs() {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000 + (uint64_t)ts.tv_nsec / 1000000;
}
static const uint32_t SHM_MAGIC   = 0x57524343u;

// e8: v2 header tail, right after the 160-byte FrameHeader (which stays
// VERBATIM — a v1 reader must keep parsing the v1 prefix).
//   active: last-published pixel bank (0|1). Written in the same seqlock
//           critical section as the rest, so a stable-seq header read pairs
//           it with that frame's pixels.
//   releasedSeq[i]: "reader has consumed the frame last published into
//           bank i up to this seq; bank i is reusable". Written by the
//           READER process (pwrite into this file) — the only cross-process
//           writer-to-reader channel. Monotonic; the words persist across
//           reader restarts, which is exactly what keeps a reconnected
//           wayrdp from deadlocking the plugin.
//   caps:   what this plugin build supports (CAP_* bits), stamped with
//           every frame so a reader can tell an old .so from a new one.
//   cursorReq: CURSOR_REQ_CLIENT = the reader's client draws the cursor
//           itself; Hyprland's software cursor is then left out of this
//           output's frames. READER-written, anything else = painted.
struct HdrTail {
    uint32_t active;        // @160, plugin-written
    uint32_t tailPad;       // @164, 8-byte alignment
    uint64_t releasedSeq[2];// @168/@176, READER-written (reader pwrite)
    uint32_t caps;          // @184, plugin-written
    uint32_t cursorReq;     // @188, READER-written (reader pwrite)
    uint8_t  reserved[256 - 192];
};
static_assert(sizeof(HdrTail) + sizeof(FrameHeader) == 256,
              "e8 v2 header must be 256 bytes (offsets are a wire format)");

// sizeof(FrameHeader) = 160 (v1). Pixels start at 256 in v2 so the v1
// rect array never collides with the handshake words. The Rust reader
// computes its pixel offset adaptively from version + file size — do not
// change one side without the other.
static const size_t HEADER_SIZE = 256;

static const uint32_t CAP_CLIENT_CURSOR = 1u << 0;
static const uint32_t CURSOR_REQ_CLIENT = 0x52534355u; // "UCSR"

// Cursor image shm (/wayrdp-cursor-<mon>): Hyprland's current cursor image,
// rewritten on every cursor change while the reader's client draws the
// cursor. Pixels are the cursor buffer's ARGB8888 rows (premultiplied, as
// Wayland shm cursors are), tightly packed, right after the 64-byte header.
// seq is a seqlock like the frame header's. Hidden cursor = visible 0.
struct CursorHeader {
    uint32_t magic;   // CURSOR_MAGIC
    uint32_t version; // 1
    uint64_t seq;     // odd = write in progress
    uint32_t width;
    uint32_t height;
    int32_t  hotX;
    int32_t  hotY;
    uint32_t visible;
    uint32_t srcWidth;  // cursor buffer size before scaling (diagnostic)
    uint32_t srcHeight;
    uint32_t scale100;  // cursor buffer scale x100 (diagnostic)
    uint8_t  pad[64 - 48];
};
static_assert(sizeof(CursorHeader) == 64, "cursor header is a wire format");
static const uint32_t CURSOR_MAGIC    = 0x57435552u; // "RUCW" on the wire
static const uint32_t CURSOR_MAX      = 256;         // larger cursors keep the last shape
static const size_t   CURSOR_SHM_SIZE = sizeof(CursorHeader) + size_t(CURSOR_MAX) * CURSOR_MAX * 4;

static HdrTail* tailOf(const Target& t) {
    return reinterpret_cast<HdrTail*>(reinterpret_cast<char*>(t.shmPtr) +
                                      sizeof(FrameHeader));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static bool isWayrdpMonitor(const PHLMONITOR& m) {
    return m && m->m_name.starts_with(g_targetPrefix);
}

static void shmTeardown(Target& t);
static void cursorShmTeardown(Target& t);

static void onMonitorDestroy(PHLMONITOR m) {
    if (!isWayrdpMonitor(m))
        return;
    // Reconnects mint fresh wayrdp-N outputs; without this hook each
    // session's shm (9+ MB) would survive until Hyprland exits.
    auto t = g_targets.find(m->m_name);
    if (t == g_targets.end())
        return;
    Log::logger->log(Log::INFO,
                     "[wayrdp-capture] monitor {} destroyed; tearing down {} shm",
                     m->m_name, t->second.shmPath);
    shmTeardown(t->second);
    cursorShmTeardown(t->second);
    g_targets.erase(t);
}

static void shmTeardown(Target& t) {
    if (t.shmPtr && t.shmPtr != MAP_FAILED) {
        munmap(t.shmPtr, t.shmSize);
        t.shmPtr = nullptr;
    }
    if (t.shmFd >= 0) {
        close(t.shmFd);
        t.shmFd = -1;
    }
    unlink(t.shmPath);
    t.shmSize = 0;
    t.frameW  = 0;
    t.frameH  = 0;
}

// Create `path` (a regular file under the 0700 runtime dir) exclusively;
// returns the fd or -1.
static int shmCreateOwned(const char* path) {
    if (!path || !*path) {
        Log::logger->log(Log::ERR,
                         "[wayrdp-capture] no XDG_RUNTIME_DIR; nowhere to create the capture segment");
        return -1;
    }
    int fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd >= 0)
        return fd;
    if (errno != EEXIST) {
        Log::logger->log(Log::ERR, "[wayrdp-capture] open {} failed: {}", path, strerror(errno));
        return -1;
    }
    // Stale segment from a previous plugin run? Take it over so the new
    // instance owns a fresh file. Ownership-gated: anything not owned by
    // us is treated as a hard failure, never silently unlinked.
    // The runtime dir is user-private, so a foreign owner means something
    // is very wrong — refuse rather than follow (O_NOFOLLOW rejects
    // symlinks outright so the uid check below can never be satisfied by
    // a link we followed).
    fd = open(path, O_RDWR | O_NOFOLLOW, 0600);
    struct stat st {};
    if (fd < 0 || fstat(fd, &st) < 0 || st.st_uid != getuid()) {
        if (fd >= 0)
            close(fd);
        Log::logger->log(Log::ERR,
                         "[wayrdp-capture] segment {} exists and is not ours; refusing takeover",
                         path);
        return -1;
    }
    close(fd);
    unlink(path);
    fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0)
        Log::logger->log(Log::ERR, "[wayrdp-capture] open {} failed: {}", path, strerror(errno));
    return fd;
}

static bool shmSetup(Target& t, uint32_t w, uint32_t h) {
    // e8: TWO pixel banks behind a 256-byte header. The plugin always
    // writes the idle bank and publishes its index; the reader hands the
    // encoder a live view into a published bank and signals release back
    // through the header. Zero big copies on either side.
    size_t need = HEADER_SIZE + 2 * size_t(w) * h * 4;
    if (t.shmPtr && t.frameW == w && t.frameH == h)
        return true;
    // Backoff guard (see Target::setupRetryAtMs): a failed setup must not
    // retry on every rendered frame. A geometry change resets it.
    const uint64_t now = nowMonoMs();
    if (t.setupRetryAtMs != 0 && now < t.setupRetryAtMs && t.retryW == w && t.retryH == h)
        return false;
    t.setupRetryAtMs = 0;

    // The reader's cursor request outlives a resize: it re-asserts only when
    // it reopens, and until then the cursor would be drawn twice.
    const uint32_t cursorReq = t.shmPtr ? tailOf(t)->cursorReq : 0;
    shmTeardown(t);

    int fd = shmCreateOwned(t.shmPath);
    if (fd < 0) {
        // Any setup failure takes the backoff (covers the no-runtime-dir
        // refusal too, so its ERR fires once per window, not per frame).
        t.setupRetryAtMs = now + SETUP_RETRY_BACKOFF_MS;
        t.retryW = w;
        t.retryH = h;
        return false;
    }
    // Commit the pages NOW, not on first write. The runtime dir is a size-
    // capped tmpfs (logind defaults RuntimeDirectorySize to 10% of RAM, vs
    // ~50% for /dev/shm): ftruncate only sets the size and succeeds past
    // the cap, and the ENOSPC would otherwise surface later as a SIGBUS
    // while the plugin writes pixels — inside Hyprland's process, killing
    // the compositor. posix_fallocate turns that into a clean setup-time
    // failure we log once and back off from (the reader side keeps
    // polling; a session that wants video can restart with the default
    // ext-image-copy capture).
    // ftruncate sets errno and returns -1; posix_fallocate returns the
    // error number itself — normalize to an errno value for strerror.
    int rc = ftruncate(fd, (off_t)need) ? errno : 0;
    if (rc == 0)
        rc = posix_fallocate(fd, 0, (off_t)need);
    if (rc != 0) {
        Log::logger->log(Log::ERR,
                         "[wayrdp-capture] allocating {} bytes for {} failed (runtime dir full?): {}",
                         need, t.shmPath, strerror(rc));
        close(fd);
        unlink(t.shmPath);
        t.setupRetryAtMs = now + SETUP_RETRY_BACKOFF_MS;
        t.retryW = w;
        t.retryH = h;
        return false;
    }
    void* p = mmap(nullptr, need, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) {
        Log::logger->log(Log::ERR, "[wayrdp-capture] mmap failed: {}", strerror(errno));
        close(fd);
        unlink(t.shmPath);
        t.setupRetryAtMs = now + SETUP_RETRY_BACKOFF_MS;
        t.retryW = w;
        t.retryH = h;
        return false;
    }

    // A fresh O_CREAT|O_EXCL file guarantees zero-filled contents, so the
    // reader's releasedSeq words are already 0; be explicit anyway — a
    // stale non-zero word would stall the first two publishes by up to
    // 16ms.
    std::memset(reinterpret_cast<char*>(p) + sizeof(FrameHeader), 0,
                HEADER_SIZE - sizeof(FrameHeader));
    reinterpret_cast<HdrTail*>(reinterpret_cast<char*>(p) + sizeof(FrameHeader))
        ->cursorReq = cursorReq;

    t.shmFd   = fd;
    t.shmPtr  = p;
    t.shmSize = need;
    t.frameW  = w;
    t.frameH  = h;
    // Fresh file ⇒ fresh handshake state. (A same-geometry reconnect takes
    // the early-return above and keeps its counters, which is correct:
    // releasedSeq words in the file are still that session's.)
    t.writeSlot      = 0;
    t.publishedSeq[0] = t.publishedSeq[1] = 0;

    Log::logger->log(Log::INFO,
                     "[wayrdp-capture] shm setup {}x{} ({} bytes) on {}",
                     w, h, need, t.shmPath);
    return true;
}

static void cursorShmTeardown(Target& t) {
    if (t.curPtr) {
        munmap(t.curPtr, CURSOR_SHM_SIZE);
        t.curPtr = nullptr;
    }
    if (t.curFd >= 0) {
        close(t.curFd);
        t.curFd = -1;
        unlink(t.curPath);
    }
}

static bool cursorShmSetup(Target& t) {
    if (t.curPtr)
        return true;
    int fd = shmCreateOwned(t.curPath);
    if (fd < 0)
        return false;
    // Same as shmSetup: commit the pages at setup so a full runtime dir is
    // a clean error here, never a SIGBUS in the render path. (This runs on
    // cursor-change events, not per frame, so no backoff is needed.)
    // ftruncate sets errno/-1; posix_fallocate returns the error number.
    int rc = ftruncate(fd, (off_t)CURSOR_SHM_SIZE) ? errno : 0;
    if (rc == 0)
        rc = posix_fallocate(fd, 0, (off_t)CURSOR_SHM_SIZE);
    if (rc != 0) {
        Log::logger->log(Log::ERR,
                         "[wayrdp-capture] cursor segment alloc failed (runtime dir full?): {}",
                         strerror(rc));
        close(fd);
        unlink(t.curPath);
        return false;
    }
    void* p = mmap(nullptr, CURSOR_SHM_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) {
        Log::logger->log(Log::ERR, "[wayrdp-capture] cursor mmap failed: {}", strerror(errno));
        close(fd);
        unlink(t.curPath);
        return false;
    }
    t.curFd  = fd;
    t.curPtr = p;
    t.curSeq = 0;
    return true;
}

// Box-filter premultiplied ARGB8888 rows from sw x sh down to dw x dh
// (dw <= sw, dh <= sh). XRGB sources count as opaque.
static void downscaleCursor(const uint8_t* src, size_t stride, uint32_t sw, uint32_t sh,
                            bool opaque, uint8_t* dst, uint32_t dw, uint32_t dh) {
    for (uint32_t dy = 0; dy < dh; dy++) {
        const uint32_t y0 = dy * sh / dh, y1 = std::max(y0 + 1, (dy + 1) * sh / dh);
        for (uint32_t dx = 0; dx < dw; dx++) {
            const uint32_t x0 = dx * sw / dw, x1 = std::max(x0 + 1, (dx + 1) * sw / dw);
            uint32_t       sum[4] = {0, 0, 0, 0};
            for (uint32_t y = y0; y < y1; y++)
                for (uint32_t x = x0; x < x1; x++) {
                    const uint8_t* s = src + y * stride + size_t(x) * 4;
                    sum[0] += s[0];
                    sum[1] += s[1];
                    sum[2] += s[2];
                    sum[3] += opaque ? 0xff : s[3];
                }
            const uint32_t n = (y1 - y0) * (x1 - x0);
            uint8_t*       d = dst + (size_t(dy) * dw + dx) * 4;
            for (int c = 0; c < 4; c++)
                d[c] = (uint8_t)((sum[c] + n / 2) / n);
        }
    }
}

// Publish Hyprland's current cursor image at its logical size: Hyprland
// renders cursors at the buffer scale (48 px for a 24 px theme) and draws
// them scaled down, so the client must get the scaled-down image (the
// hotspot is already logical). A cursor buffer we cannot read (not
// CPU-mappable, unexpected format, oversized) keeps the last shape.
static bool writeCursor(Target& t) {
    if (!cursorShmSetup(t))
        return false;
    const auto& img = Pointer::mgr()->currentCursorImage();
    auto*       hdr = reinterpret_cast<CursorHeader*>(t.curPtr);
    auto*       px  = reinterpret_cast<uint8_t*>(t.curPtr) + sizeof(CursorHeader);

    uint8_t* data   = nullptr;
    uint32_t fmt    = 0;
    size_t   stride = 0;
    uint32_t w = 0, h = 0;
    if (img.pBuffer) {
        w = (uint32_t)img.pBuffer->size.x;
        h = (uint32_t)img.pBuffer->size.y;
        if (w == 0 || h == 0 || w > CURSOR_MAX || h > CURSOR_MAX)
            return false;
        std::tie(data, fmt, stride) = img.pBuffer->beginDataPtr(0);
        if (!data || stride < size_t(w) * 4 ||
            (fmt != DRM_FORMAT_ARGB8888 && fmt != DRM_FORMAT_XRGB8888)) {
            if (data)
                img.pBuffer->endDataPtr();
            return false;
        }
    }

    const float    scale = img.scale > 1.F ? img.scale : 1.F;
    const uint32_t dw    = std::max<uint32_t>(1, (uint32_t)std::lround(w / scale));
    const uint32_t dh    = std::max<uint32_t>(1, (uint32_t)std::lround(h / scale));

    std::atomic_ref<uint64_t>(hdr->seq).store(t.curSeq + 1, std::memory_order_release);
    if (data) {
        downscaleCursor(data, stride, w, h, fmt == DRM_FORMAT_XRGB8888, px, dw, dh);
        img.pBuffer->endDataPtr();
    }
    t.curSeq += 2;
    hdr->magic     = CURSOR_MAGIC;
    hdr->version   = 1;
    hdr->width     = data ? dw : 0;
    hdr->height    = data ? dh : 0;
    hdr->srcWidth  = w;
    hdr->srcHeight = h;
    hdr->scale100  = (uint32_t)std::lround(img.scale * 100);
    hdr->hotX    = (int32_t)img.hotspot.x;
    hdr->hotY    = (int32_t)img.hotspot.y;
    hdr->visible = data ? 1 : 0;
    std::atomic_ref<uint64_t>(hdr->seq).store(t.curSeq, std::memory_order_release);
    return true;
}

static void sockSetup() {
    if (g_sockFd >= 0)
        return;
    int fd = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    if (fd < 0) {
        Log::logger->log(Log::ERR, "[wayrdp-capture] socket failed: {}", strerror(errno));
        return;
    }
    g_sockFd = fd;
    Log::logger->log(Log::INFO, "[wayrdp-capture] sock ready");
}

static void sockNotify(Target& t) {
    if (g_sockFd < 0 || !t.sockPath[0])
        return;
    struct sockaddr_un addr {};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, t.sockPath, sizeof(addr.sun_path) - 1);
    ssize_t n = sendto(g_sockFd, &t.seq, sizeof(t.seq), 0,
                       (struct sockaddr*)&addr, sizeof(addr));
    if (n < 0 && (errno == ENOENT || errno == ECONNREFUSED || errno == EAGAIN)) {
        t.dropNoPeer++;
        return;
    }
}

// ---------------------------------------------------------------------------
// Client-drawn cursor
// ---------------------------------------------------------------------------

// Hand every client-drawn target the new cursor image and wake its reader.
static void onCursorChanged() {
    for (auto& [name, t] : g_targets) {
        if (t.clientCursor && writeCursor(t)) {
            sockSetup();
            sockNotify(t);
        }
    }
}

// Hyprland composites its software cursor into the output framebuffer before
// RENDER_LAST_MOMENT, so the only way to keep it out of the captured frames is
// to not draw it on that output. Two trampoline hooks on CPointerManager do
// that for wayrdp outputs whose reader asked for it (HdrTail::cursorReq);
// every other output and every screencopy render (screencopy = true, e.g.
// cursor-session images) goes through untouched.
static CFunctionHook* g_renderSwCursorsHook = nullptr;
static CFunctionHook* g_damageCursorHook    = nullptr;

static bool clientDrawsCursor(const PHLMONITOR& m) {
    if (!isWayrdpMonitor(m))
        return false;
    auto it = g_targets.find(m->m_name);
    if (it == g_targets.end() || !it->second.shmPtr)
        return false;
    return std::atomic_ref<uint32_t>(tailOf(it->second)->cursorReq)
               .load(std::memory_order_acquire) == CURSOR_REQ_CLIENT;
}

typedef void (*RenderSwCursorsFn)(Pointer::CPointerManager*, PHLMONITOR, const Time::steady_tp&,
                                  CRegion&, std::optional<Vector2D>, bool, bool);
static void hkRenderSoftwareCursorsFor(Pointer::CPointerManager* self, PHLMONITOR m,
                                       const Time::steady_tp& now, CRegion& damage,
                                       std::optional<Vector2D> overridePos,
                                       bool screencopy, bool forceRender) {
    if (!screencopy && clientDrawsCursor(m))
        return;
    ((RenderSwCursorsFn)g_renderSwCursorsHook->m_original)(self, m, now, damage, overridePos,
                                                            screencopy, forceRender);
}

// Cursor motion damages the cursor box and schedules a frame. With the
// cursor not drawn there is nothing to repair, and skipping it keeps pointer
// motion from producing (full-surface encoded) frames at all.
typedef void (*DamageCursorFn)(Pointer::CPointerManager*, PHLMONITOR, bool);
static void hkDamageCursor(Pointer::CPointerManager* self, PHLMONITOR m, bool skipFrameSchedule) {
    if (clientDrawsCursor(m))
        return;
    ((DamageCursorFn)g_damageCursorHook->m_original)(self, m, skipFrameSchedule);
}

// Exactly one match whose demangled name is Pointer::CPointerManager::<name>(...), so
// an overload or a same-named method elsewhere can never be hooked.
static CFunctionHook* hookPointerManager(const std::string& name, const void* dest) {
    void* addr = nullptr;
    for (const auto& f : HyprlandAPI::findFunctionsByName(g_pHandle, name)) {
        if (!f.demangled.starts_with("Pointer::CPointerManager::" + name + "("))
            continue;
        if (addr) {
            Log::logger->log(Log::ERR, "[wayrdp-capture] {} is ambiguous; not hooking", name);
            return nullptr;
        }
        addr = f.address;
    }
    if (!addr) {
        Log::logger->log(Log::ERR, "[wayrdp-capture] CPointerManager::{} not found", name);
        return nullptr;
    }
    auto* hook = HyprlandAPI::createFunctionHook(g_pHandle, addr, dest);
    if (hook && hook->hook())
        return hook;
    if (hook)
        HyprlandAPI::removeFunctionHook(g_pHandle, hook);
    Log::logger->log(Log::ERR, "[wayrdp-capture] hooking CPointerManager::{} failed", name);
    return nullptr;
}

static void removeCursorHooks() {
    for (auto** h : {&g_renderSwCursorsHook, &g_damageCursorHook}) {
        if (*h)
            HyprlandAPI::removeFunctionHook(g_pHandle, *h);
        *h = nullptr;
    }
    g_cursorHooked = false;
}

// Both hooks or neither: a hidden cursor that still damages (or the reverse)
// is worse than the painted one. Without them caps stays 0 and readers keep
// the painted cursor.
static void installCursorHooks() {
    g_renderSwCursorsHook = hookPointerManager("renderSoftwareCursorsFor",
                                               (void*)&hkRenderSoftwareCursorsFor);
    g_damageCursorHook    = hookPointerManager("damageCursor", (void*)&hkDamageCursor);
    g_cursorHooked        = g_renderSwCursorsHook && g_damageCursorHook;
    if (!g_cursorHooked)
        removeCursorHooks();
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

// A capture armed at RENDER_LAST_MOMENT, read after endRender (see e9 below).
struct PendingCapture {
    bool          armed = false;
    PHLMONITORREF mon;
    GLint         fb = -1; // out FB of the armed frame; -1 = the bound READ FB
    FrameRect     rects[MAX_DAMAGE_RECTS] = {};
    uint32_t      nrects = 0;
};
static PendingCapture g_pending;
static CFunctionHook* g_endRenderHook = nullptr;

static void onRenderPre(PHLMONITOR m) {
    g_pending.armed = false; // a render that never reached endRender
    g_currentMon  = m;
    g_inWayrdpMon = isWayrdpMonitor(m);
    if (!g_inWayrdpMon)
        return;
    auto it = g_targets.find(m->m_name);
    if (it == g_targets.end())
        return;
    const bool client = clientDrawsCursor(m);
    if (client != it->second.clientCursor) {
        // The cursor just left (or rejoined) the frames: repaint the whole
        // output once so its last painted image does not linger.
        it->second.clientCursor = client;
        g_pHyprRenderer->damageMonitor(m);
        if (client)
            onCursorChanged();
        Log::logger->log(Log::INFO, "[wayrdp-capture] {}: cursor {}", m->m_name,
                         client ? "drawn by the client" : "painted");
    }
}

// e9: read the frame AFTER Hyprland has drawn it. Hyprland 0.56 records the
// frame's elements into a CRenderPass and only executes it inside
// endRender(), so at RENDER_LAST_MOMENT the render target is still empty and
// the swapchain buffer bound for reading holds the frame from one swapchain
// cycle ago (3 renders: measured on O6 as ~52 ms of commit→capture lag). So
// RENDER_LAST_MOMENT only arms a capture — this frame's damage and its out
// FB — and a hook on CHyprGLRenderer::endRender reads that FB once the pass
// has run and been copied out (the GL context is still current there).
// Without the hook the readback stays at RENDER_LAST_MOMENT (one cycle late).
// (PendingCapture / g_pending above.)

static GLint glFbId(const SP<Render::IFramebuffer>& fb) {
    auto* gl = dynamic_cast<Render::GL::CGLFramebuffer*>(fb.get());
    return gl ? (GLint)gl->getFBID() : -1;
}

static void captureFrame(const PHLMONITOR& mon, GLint readFb, const FrameRect* frameRects,
                         uint32_t frameRectCount);

// e2a: this frame's compositor damage. NOT from m_damage directly —
// beginRender() (getBufferDamage + rotate) ran before RENDER_LAST_MOMENT
// fires, so m_damage.m_current is already cleared here. The frame's
// damage lives in the renderer's m_renderData.damage: it IS the value
// beginRender pulled, clipped to monitor bounds by the damage ring —
// except it is NOT capped at 8 rects (Hyprland caps getBufferDamage
// output, but extra damage added to renderData.damage during the pass
// bypasses that), so we cap ourselves: >8 collapses to full-surface,
// partial damage being worse than full. Coords are buffer-space
// (top-left origin, scale included) — matching glReadPixels rows.
// Empty damage is NOT "no change" (blur paths damage the pass but not
// the ring; the monitor only renders when something changed), so a
// zero-rect read publishes full-surface instead.
// nrects == 0 (empty region, all-degenerate, monitor mismatch, or the
// >8 collapse) → header publishes full-surface.
static uint32_t collectDamage(FrameRect (&rects)[MAX_DAMAGE_RECTS]) {
    uint32_t nrects = 0;
    if (g_pHyprRenderer->m_renderData.pMonitor == g_currentMon) {
        const auto& dmg = g_pHyprRenderer->m_renderData.damage;
        auto        boxes = dmg.getRects();
        if (boxes.size() <= MAX_DAMAGE_RECTS) {
            for (const auto& b : boxes) {
                if (b.x2 <= b.x1 || b.y2 <= b.y1)
                    continue;
                rects[nrects++] = FrameRect{(uint32_t)b.x1, (uint32_t)b.y1,
                                            (uint32_t)(b.x2 - b.x1),
                                            (uint32_t)(b.y2 - b.y1)};
            }
        }
    }
    return nrects;
}

static void onRenderStage(eRenderStage stage) {
    if (!g_inWayrdpMon || !g_currentMon)
        return;
    if (stage != RENDER_LAST_MOMENT)
        return;

    g_pending        = {};
    g_pending.nrects = collectDamage(g_pending.rects);
    if (g_endRenderHook) {
        g_pending.armed = true;
        g_pending.mon   = g_currentMon;
        g_pending.fb    = glFbId(g_pHyprRenderer->m_renderData.outFB);
        return;
    }
    captureFrame(g_currentMon, -1, g_pending.rects, g_pending.nrects);
}

typedef void (*EndRenderFn)(void*, const std::function<void()>&);
static void hkEndRender(void* self, const std::function<void()>& renderingDone) {
    // Only the endRender of the armed monitor's own render: any other
    // (screencopy, snapshot, another output) leaves the pending capture alone.
    const bool ours = g_pending.armed && g_pending.mon &&
                      g_pHyprRenderer->m_renderData.pMonitor == g_pending.mon;
    ((EndRenderFn)g_endRenderHook->m_original)(self, renderingDone);
    if (!ours)
        return;
    g_pending.armed = false;
    if (auto mon = g_pending.mon.lock())
        captureFrame(mon, g_pending.fb, g_pending.rects, g_pending.nrects);
}

static void installEndRenderHook() {
    void* addr = nullptr;
    for (const auto& f : HyprlandAPI::findFunctionsByName(g_pHandle, "endRender")) {
        if (!f.demangled.starts_with("Render::GL::CHyprGLRenderer::endRender("))
            continue;
        if (addr) {
            Log::logger->log(Log::ERR, "[wayrdp-capture] endRender is ambiguous; not hooking");
            return;
        }
        addr = f.address;
    }
    if (!addr) {
        Log::logger->log(Log::ERR, "[wayrdp-capture] CHyprGLRenderer::endRender not found");
        return;
    }
    auto* hook = HyprlandAPI::createFunctionHook(g_pHandle, addr, (void*)&hkEndRender);
    if (hook && hook->hook()) {
        g_endRenderHook = hook;
        return;
    }
    if (hook)
        HyprlandAPI::removeFunctionHook(g_pHandle, hook);
    Log::logger->log(Log::ERR, "[wayrdp-capture] hooking CHyprGLRenderer::endRender failed");
}

static void captureFrame(const PHLMONITOR& mon, GLint readFb, const FrameRect* frameRects,
                         uint32_t frameRectCount) {
    const auto w = (uint32_t)mon->m_size.x;
    const auto h = (uint32_t)mon->m_size.y;
    if (w == 0 || h == 0)
        return;

    auto [it, fresh] = g_targets.try_emplace(mon->m_name, mon->m_name);
    Target& t = it->second;
    if (fresh)
        Log::logger->log(Log::INFO,
                         "[wayrdp-capture] new target for monitor {}",
                         mon->m_name);

    if (!shmSetup(t, w, h)) {
        // shmSetup logged the failure (once per backoff window) and set
        // the backoff; nothing to add per frame.
        return;
    }
    sockSetup();

    // e8 double-buffer handshake: this frame goes into the bank opposite
    // the one last published. Reuse rule: the reader pwrites
    // releasedSeq[bank] on drop of its view; a bank is free once that word
    // reaches the seq last published there. BUSY → drop THIS frame without
    // sleeping (encoder-speed rate limiting; the render loop must never
    // wait on a network thread). Two degenerate cases stay healthy:
    //   no reader yet   – words stay 0; after the first pair of publishes
    //                     everything drops until a reader attaches (attach
    //                     stamps both words UINT64_MAX = "nothing held");
    //   reader died     – held bank locks up, the other keeps cycling;
    //                     reconnect self-heals via the same attach stamp.
    auto* hdr  = reinterpret_cast<FrameHeader*>(t.shmPtr);
    auto* tail = reinterpret_cast<HdrTail*>(reinterpret_cast<char*>(t.shmPtr) +
                                            sizeof(FrameHeader));
    const uint32_t bank = t.writeSlot;
    if (t.publishedSeq[bank] != 0 &&
        std::atomic_ref<uint64_t>(
            *const_cast<uint64_t*>(&tail->releasedSeq[bank]))
                .load(std::memory_order_acquire) < t.publishedSeq[bank]) {
        t.dropSlotBusy++;
        if (t.dropSlotBusy == 1 || t.dropSlotBusy % 600 == 0)
            Log::logger->log(
                Log::WARN,
                "[wayrdp-capture] bank busy on {}: reader still holds seq {}; "
                "frame dropped (total {})",
                mon->m_name, t.publishedSeq[bank], t.dropSlotBusy);
        return; // writeSlot NOT toggled: retry this bank next frame
    }
    t.writeSlot ^= 1; // publish happens → next frame lands in the other bank

    // Seqlock discipline (cross-process, reader polls): odd seq = write in
    // progress, even seq = stable frame. With v2 the seqlock protects ONLY
    // the 256-byte header — the reader's live view is the OTHER bank.
    // glReadPixels is a synchronous pipeline stall, so this thread is the
    // only writer and a single bump pair suffices; the atomic ordering is
    // what matters.
    // release (not relaxed): on ARM a relaxed store can sink below the pixel
    // writes that follow it, letting a reader see odd-seq AFTER pixels were
    // already written and treat the frame as stable mid-write. Costs ~0.
    std::atomic_ref<uint64_t>(hdr->seq).store(t.seq + 1, std::memory_order_release);

    // Read the frame's out FB (readFb, from the endRender hook) or, on the
    // fallback path, whatever is bound for reading at RENDER_LAST_MOMENT.
    GLubyte* pixels = reinterpret_cast<GLubyte*>(t.shmPtr) + HEADER_SIZE +
                      (size_t)bank * t.frameW * t.frameH * 4;
    GLint prevReadFb = 0;
    if (readFb >= 0) {
        glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &prevReadFb);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)readFb);
    }
    glPixelStorei(GL_PACK_ALIGNMENT, 4);
    glReadPixels(0, 0, (GLsizei)t.frameW, (GLsizei)t.frameH,
                 GL_BGRA_EXT, GL_UNSIGNED_BYTE, pixels);
    if (readFb >= 0)
        glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)prevReadFb);

    // Publish AFTER the pixel write completes. The pad stamp, damage rects
    // and the active-bank selector go in the same critical section so the
    // seqlock pairs them with these pixels.
    t.seq += 2;
    hdr->magic       = SHM_MAGIC;
    hdr->width       = t.frameW;
    hdr->height      = t.frameH;
    hdr->pad         = (uint32_t)nowMonoMs();
    hdr->version     = HDR_VERSION;
    hdr->rect_count  = frameRectCount;
    std::memcpy(hdr->rects, frameRects, sizeof(hdr->rects));
    tail->active     = bank;
    tail->caps       = g_cursorHooked ? CAP_CLIENT_CURSOR : 0;
    t.publishedSeq[bank] = t.seq;
    std::atomic_ref<uint64_t>(hdr->seq).store(t.seq, std::memory_order_release);

    sockNotify(t);

    g_frameSeq++;
    if (t.seq <= 2 || t.seq % 600 == 0) {
        Log::logger->log(Log::INFO,
                         "[wayrdp-capture] frame #{} {}x{} on {} (drops={}, bank-busy={})",
                         t.seq, t.frameW, t.frameH,
                         mon->m_name, t.dropNoPeer, t.dropSlotBusy);
    }
}

// ---------------------------------------------------------------------------
// Plugin entry
// ---------------------------------------------------------------------------

APICALL EXPORT std::string pluginAPIVersion() {
    return HYPRLAND_API_VERSION;
}

APICALL EXPORT PLUGIN_DESCRIPTION_INFO pluginInit(HANDLE handle) {
    g_pHandle = handle;

    const std::string compiledAgainst = __hyprland_api_get_client_hash();
    const std::string runtimeHash     = __hyprland_api_get_hash();
    if (compiledAgainst != runtimeHash) {
        // Hyprland has no stable plugin ABI: a stale .so means UB *inside the
        // compositor*, which takes the whole login session down. Refuse to
        // load — Hyprland unloads us on the throw and ext-image-copy capture
        // remains as the fallback path.
        throw std::runtime_error(
            "[wayrdp-capture] Hyprland API hash mismatch; refusing to load "
            "(rebuild the plugin against the installed Hyprland)");
    }

    auto& bus = Event::bus();

    g_onRenderPre = bus->m_events.render.pre.listen(
        [](PHLMONITOR m) { onRenderPre(m); });

    g_onRenderStage = bus->m_events.render.stage.listen(
        [](eRenderStage s) { onRenderStage(s); });

    g_onMonitorDestroy = bus->m_events.monitor.destroyMon.listen(
        [](PHLMONITOR m) { onMonitorDestroy(m); });

    installEndRenderHook();
    installCursorHooks();
    if (g_cursorHooked)
        g_onCursorChanged = Pointer::mgr()->m_events.cursorChanged.listen([] { onCursorChanged(); });

    Log::logger->log(Log::INFO, "[wayrdp-capture] loaded, targeting prefix={}, client cursor {}, readback {}",
                     g_targetPrefix, g_cursorHooked ? "supported" : "unsupported",
                     g_endRenderHook ? "after endRender" : "at RENDER_LAST_MOMENT (one swapchain cycle late)");

    return {"wayrdp-capture",
            "Direct-render capture for wayrdp (skips ext-image-copy paint)",
            "VMCreate",
            "0.1"};
}

APICALL EXPORT void pluginExit() {
    Log::logger->log(Log::INFO,
                     "[wayrdp-capture] unloaded after {} frames",
                     g_frameSeq);
    g_onRenderStage      = {};
    g_onRenderPre        = {};
    g_onMonitorDestroy   = {};
    g_onCursorChanged    = {};
    removeCursorHooks();
    if (g_endRenderHook)
        HyprlandAPI::removeFunctionHook(g_pHandle, g_endRenderHook);
    g_endRenderHook = nullptr;
    g_pending       = {};

    if (g_sockFd >= 0) {
        close(g_sockFd);
        g_sockFd = -1;
    }
    for (auto& [name, t] : g_targets) {
        shmTeardown(t);
        cursorShmTeardown(t);
    }
    g_targets.clear();
}
