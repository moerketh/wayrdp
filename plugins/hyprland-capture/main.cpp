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
// RENDER_LAST_MOMENT (GL context still bound). We glReadPixels from the
// already-painted output FBO into a memfd, then notify via a Unix
// datagram socket. wayrdp consumes the shm.
//
// Cost per frame: one FB readPixels (~1-3ms) vs 40ms for fake render.

#include <hyprland/src/Compositor.hpp>
#include <hyprland/src/debug/log/Logger.hpp>
#include <hyprland/src/event/EventBus.hpp>
#include <hyprland/src/helpers/Color.hpp>
#include <hyprland/src/helpers/memory/Memory.hpp>
#include <hyprland/src/output/DamageRing.hpp>
#include <hyprland/src/plugins/PluginAPI.hpp>
#include <hyprland/src/render/OpenGL.hpp>
#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/version.h>

#include <hyprutils/math/Region.hpp>
#include <hyprutils/signal/Signal.hpp>
#include <hyprutils/signal/Listener.hpp>
#include <hyprutils/memory/WeakPtr.hpp>

#include <GLES3/gl32.h>

#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <fcntl.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <cstdint>
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

static std::string  g_targetPrefix = "wayrdp";
static bool         g_inWayrdpMon  = false;
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
    char     shmPath[256];
    char     sockPath[256];

    Target(const std::string& monName) {
        std::snprintf(shmPath, sizeof(shmPath),
                      "/wayrdp-capture-%s", monName.c_str());
        // Frame-notify datagram target (best-effort; nothing binds it yet).
        // Under XDG_RUNTIME_DIR — a user-private dir — not world-writable
        // /tmp, so a local user can't pre-create squatting paths.
        const char* rt = std::getenv("XDG_RUNTIME_DIR");
        const char* base = (rt && *rt) ? rt : "/tmp";
        std::snprintf(sockPath, sizeof(sockPath),
                      "%s/wayrdp-capture-%s.sock", base, monName.c_str());
    }
    Target(const Target&) = delete;
    Target& operator=(const Target&) = delete;
};

static std::unordered_map<std::string, Target> g_targets;

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
struct HdrTail {
    uint32_t active;        // @160, plugin-written
    uint32_t tailPad;       // @164, 8-byte alignment
    uint64_t releasedSeq[2];// @168/@176, READER-written (reader pwrite)
    uint8_t  reserved[256 - 184];
};
static_assert(sizeof(HdrTail) + sizeof(FrameHeader) == 256,
              "e8 v2 header must be 256 bytes (offsets are a wire format)");

// sizeof(FrameHeader) = 160 (v1). Pixels start at 256 in v2 so the v1
// rect array never collides with the handshake words. The Rust reader
// computes its pixel offset adaptively from version + file size — do not
// change one side without the other.
static const size_t HEADER_SIZE = 256;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static bool isWayrdpMonitor(const PHLMONITOR& m) {
    return m && m->m_name.starts_with(g_targetPrefix);
}

static void shmTeardown(Target& t);

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
    shm_unlink(t.shmPath);
    t.shmSize = 0;
    t.frameW  = 0;
    t.frameH  = 0;
}

static bool shmSetup(Target& t, uint32_t w, uint32_t h) {
    // e8: TWO pixel banks behind a 256-byte header. The plugin always
    // writes the idle bank and publishes its index; the reader hands the
    // encoder a live view into a published bank and signals release back
    // through the header. Zero big copies on either side.
    size_t need = HEADER_SIZE + 2 * size_t(w) * h * 4;
    if (t.shmPtr && t.frameW == w && t.frameH == h)
        return true;

    shmTeardown(t);

    int fd = shm_open(t.shmPath, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0) {
        // Stale shm from a previous plugin run? Take it over so the new
        // instance owns a fresh file. Ownership-gated: anything not owned by
        // us is treated as a hard failure, never silently unlinked.
        // /dev/shm can contain symlinks (POSIX shm objects are tmpfs files);
        // O_NOFOLLOW refuses them outright so the uid check below can never
        // be satisfied by a link we followed. (S_ISLNK on an fstat result is
        // structurally impossible — fstat reports the target — so it's gone.)
        fd = shm_open(t.shmPath, O_RDWR | O_NOFOLLOW, 0600);
        struct stat st {};
        if (fd < 0 || fstat(fd, &st) < 0 || st.st_uid != getuid()) {
            if (fd >= 0)
                close(fd);
            Log::logger->log(Log::ERR,
                             "[wayrdp-capture] shm {} exists and is not ours; refusing takeover",
                             t.shmPath);
            return false;
        }
        close(fd);
        shm_unlink(t.shmPath);
        fd = shm_open(t.shmPath, O_RDWR | O_CREAT | O_EXCL, 0600);
        if (fd < 0) {
            Log::logger->log(Log::ERR, "[wayrdp-capture] shm_open failed: {}", strerror(errno));
            return false;
        }
    }
    if (ftruncate(fd, (off_t)need) != 0) {
        Log::logger->log(Log::ERR, "[wayrdp-capture] ftruncate failed: {}", strerror(errno));
        close(fd);
        shm_unlink(t.shmPath);
        return false;
    }
    void* p = mmap(nullptr, need, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) {
        Log::logger->log(Log::ERR, "[wayrdp-capture] mmap failed: {}", strerror(errno));
        close(fd);
        shm_unlink(t.shmPath);
        return false;
    }

    // shm_open(O_EXCL) guarantees zero-filled contents, so the reader's
    // releasedSeq words are already 0; be explicit anyway — a stale
    // non-zero word would stall the first two publishes by up to 16ms.
    std::memset(reinterpret_cast<char*>(p) + sizeof(FrameHeader), 0,
                HEADER_SIZE - sizeof(FrameHeader));

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
    if (g_sockFd < 0)
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
// Hooks
// ---------------------------------------------------------------------------

static void onRenderPre(PHLMONITOR m) {
    g_currentMon  = m;
    g_inWayrdpMon = isWayrdpMonitor(m);
}

static void onRenderStage(eRenderStage stage) {
    if (!g_inWayrdpMon || !g_currentMon)
        return;
    if (stage != RENDER_LAST_MOMENT)
        return;

    const auto w = (uint32_t)g_currentMon->m_size.x;
    const auto h = (uint32_t)g_currentMon->m_size.y;
    if (w == 0 || h == 0)
        return;

    auto [it, fresh] = g_targets.try_emplace(g_currentMon->m_name,
                                             g_currentMon->m_name);
    Target& t = it->second;
    if (fresh)
        Log::logger->log(Log::INFO,
                         "[wayrdp-capture] new target for monitor {}",
                         g_currentMon->m_name);

    if (!shmSetup(t, w, h)) {
        // Log throttled, do not kill the renderer with repeated setup costs.
        static uint64_t logCnt = 0;
        if (++logCnt % 600 == 1)
            Log::logger->log(Log::ERR,
                             "[wayrdp-capture] shmSetup failed (count={})", logCnt);
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
                g_currentMon->m_name, t.publishedSeq[bank], t.dropSlotBusy);
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

    // Read the bound output FBO. At RENDER_LAST_MOMENT Hyprland has just
    // finished rendering this monitor and the GL context is still active.
    GLubyte* pixels = reinterpret_cast<GLubyte*>(t.shmPtr) + HEADER_SIZE +
                      (size_t)bank * t.frameW * t.frameH * 4;
    glPixelStorei(GL_PACK_ALIGNMENT, 4);
    glReadPixels(0, 0, (GLsizei)t.frameW, (GLsizei)t.frameH,
                 GL_BGRA_EXT, GL_UNSIGNED_BYTE, pixels);

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
    FrameRect rects[MAX_DAMAGE_RECTS] = {};
    uint32_t  nrects = 0;
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
    // nrects == 0 (empty region, all-degenerate, monitor mismatch, or the
    // >8 collapse above) → header publishes full-surface.

    // Publish AFTER the pixel write completes. The pad stamp, damage rects
    // and the active-bank selector go in the same critical section so the
    // seqlock pairs them with these pixels.
    t.seq += 2;
    hdr->magic       = SHM_MAGIC;
    hdr->width       = t.frameW;
    hdr->height      = t.frameH;
    hdr->pad         = (uint32_t)nowMonoMs();
    hdr->version     = HDR_VERSION;
    hdr->rect_count  = nrects;
    std::memcpy(hdr->rects, rects, sizeof(rects));
    tail->active     = bank;
    t.publishedSeq[bank] = t.seq;
    std::atomic_ref<uint64_t>(hdr->seq).store(t.seq, std::memory_order_release);

    sockNotify(t);

    g_frameSeq++;
    if (t.seq <= 2 || t.seq % 600 == 0) {
        Log::logger->log(Log::INFO,
                         "[wayrdp-capture] frame #{} {}x{} on {} (drops={}, bank-busy={})",
                         t.seq, t.frameW, t.frameH,
                         g_currentMon->m_name, t.dropNoPeer, t.dropSlotBusy);
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

    Log::logger->log(Log::INFO, "[wayrdp-capture] loaded, targeting prefix={}", g_targetPrefix);

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

    if (g_sockFd >= 0) {
        close(g_sockFd);
        g_sockFd = -1;
    }
    for (auto& [name, t] : g_targets) {
        shmTeardown(t);
    }
    g_targets.clear();
}
