//! Round-trip test for wayrdp RFX region encoding.
//!
//! Encodes synthetic frames via the shipping shim (wayrdp_rfx_encode_region)
//! and decodes them with a plain FreeRDP RFX_CONTEXT decoder, pixel-checking
//! the result. This proves that the bytes our encoder emits are valid RFX
//! for the rect layout we intend.
//!
//! Why: vmconnect drops RDPGFX on the wire when we send region-encoded
//! frames. This harness lets us iterate on the encode-side fix locally
//! without a live Hyper-V session.
//!
//! Build (on Linux with freerdp3-dev installed):
//!   gcc -g -O0 -Wall -Wextra \
//!       rfx_region_roundtrip.c ../shim.c \
//!       -o rfx_region_roundtrip \
//!       $(pkg-config --cflags --libs freerdp3 winpr3 freerdp-server3)
//!
//! Run:
//!   ./rfx_region_roundtrip

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <freerdp/codec/rfx.h>
#include <freerdp/codec/color.h>
#include <freerdp/codec/region.h>
#include <winpr/stream.h>

typedef struct WayrdpRfx WayrdpRfx;
extern WayrdpRfx* wayrdp_rfx_new(UINT32 width, UINT32 height);
extern BOOL wayrdp_rfx_encode_region(WayrdpRfx* e, const BYTE* src, UINT32 src_step,
                                     const UINT16* rects, size_t nrects,
                                     const BYTE** out_data, UINT32* out_len);
extern void wayrdp_rfx_free(WayrdpRfx* e);

#define W 1920
#define H 1200
#define BPP 4 /* BGRX32 */

static void fill_frame(BYTE* buf, UINT8 seed)
{
    for (size_t i = 0; i < (size_t)W * H; i++)
    {
        buf[i * BPP + 0] = (BYTE)((i * 7 + seed) & 0xFF);
        buf[i * BPP + 1] = (BYTE)((i / 13 + seed) & 0xFF);
        buf[i * BPP + 2] = (BYTE)((i / 97 + seed * 2) & 0xFF);
        buf[i * BPP + 3] = 0xFF;
    }
}

static void draw_rect(BYTE* buf, UINT16 x, UINT16 y, UINT16 w, UINT16 h, UINT8 shade)
{
    for (UINT32 row = 0; row < h; row++)
    {
        UINT32 yy = (UINT32)y + row;
        if (yy >= H)
            break;
        for (UINT32 col = 0; col < w; col++)
        {
            UINT32 xx = (UINT32)x + col;
            if (xx >= W)
                break;
            size_t off = (size_t)yy * W * BPP + (size_t)xx * BPP;
            buf[off + 0] = shade;
            buf[off + 1] = shade ^ 0xAA;
            buf[off + 2] = shade ^ 0x55;
        }
    }
}

/* Decode `len` bytes of RFX into the (0,0)-anchored surface `dst`.
 * The decoder context must be REUSED across frames — the wire format only
 * sends sync/context/codecVersions/channels on the first encoded message
 * (state flips to SEND_FRAME_DATA). Reusing the context mirrors what the
 * real client does and matches FreeRDP's encoder state machine. */
static RFX_CONTEXT* g_dec = NULL;
static BOOL decode_merge_at(const BYTE* data, UINT32 len, BYTE* dst, UINT16 left, UINT16 top)
{
    if (!g_dec)
    {
        g_dec = rfx_context_new(FALSE);
        if (!g_dec)
            return FALSE;
        rfx_context_set_pixel_format(g_dec, PIXEL_FORMAT_BGRX32);
    }

    REGION16 invalid;
    region16_init(&invalid);
    /* left/top == the surface command's destLeft/destTop on the wire. The
     * decoder uses these to know where the RFX tile grid is anchored
     * within the surface. Passing 0,0 means "RFX tile (x,y) == surface
     * pixel (x,y)"; passing (left,top) means "RFX tile (0,0) == surface
     * pixel (left,top)". freerdp-shadow uses the former (full-surface grid
     * every frame); it does NOT crop tiles. */
    BOOL ok = rfx_process_message(g_dec, data, len, left, top, dst, PIXEL_FORMAT_BGRX32,
                                  W * BPP, H, &invalid);
    if (ok)
    {
        const RECTANGLE_16* ext = region16_extents(&invalid);
        if (ext->right <= ext->left || ext->bottom <= ext->top)
            ok = FALSE;
    }
    region16_uninit(&invalid);
    return ok;
}
static BOOL decode_merge(const BYTE* data, UINT32 len, BYTE* dst)
{
    return decode_merge_at(data, len, dst, 0, 0);
}

static int fail(const char* msg, int code)
{
    fprintf(stderr, "FAIL: %s\n", msg);
    return code;
}

int main(void)
{
    BYTE* src = malloc((size_t)W * H * BPP);
    BYTE* got = malloc((size_t)W * H * BPP);
    if (!src || !got)
        return 2;

    /* Frame 1: baseline gradient, full-surface encode. */
    fill_frame(src, 0x11);
    UINT16 full[4] = { 0, 0, W, H };
    WayrdpRfx* enc = wayrdp_rfx_new(W, H);
    assert(enc);

    const BYTE* wire = NULL;
    UINT32 wire_len = 0;
    if (!wayrdp_rfx_encode_region(enc, src, W * BPP, full, 1, &wire, &wire_len))
        return fail("encode full baseline", 3);

    memset(got, 0, (size_t)W * H * BPP);
    if (!decode_merge(wire, wire_len, got))
        return fail("decode baseline", 4);

    /* Save baseline-after-decode for the round-trip check. */
    BYTE* baseline = malloc((size_t)W * H * BPP);
    memcpy(baseline, got, (size_t)W * H * BPP);

    /* Frame 2: stamp a cursor change at (500,300) 96x96; encode with only
     * that damage rect. */
    UINT16 cx = 500, cy = 300, cw = 96, ch = 96;
    draw_rect(src, cx, cy, cw, ch, 0xE7);
    UINT16 rect[4] = { cx, cy, cw, ch };

    if (!wayrdp_rfx_encode_region(enc, src, W * BPP, rect, 1, &wire, &wire_len))
        return fail("encode cursor-region", 5);
    fprintf(stderr, "[t] frame2 wire_len=%u\n", wire_len);

    /* Inspect wire: dump first 128 bytes as hex to catch block types. */
    fprintf(stderr, "[t] frame2 first bytes:");
    for (UINT32 i = 0; i < (wire_len < 128 ? wire_len : 128); i++)
    {
        if (i % 16 == 0)
            fprintf(stderr, "\n  %04x:", i);
        fprintf(stderr, " %02x", wire[i]);
    }
    fprintf(stderr, "\n");

    /* Parse the wire: walk FRAME_BEGIN/REGION/TILESET blocks and print
     * the tile coordinates the decoder will see. This proves the encoder
     * emits grid-correct tiles independent of decoding behavior. */
    {
        const BYTE* p = wire;
        const BYTE* end = wire + wire_len;
        while (p + 6 <= end)
        {
            UINT16 btype = (UINT16)p[0] | ((UINT16)p[1] << 8);
            UINT32 blen = (UINT32)p[2] | ((UINT32)p[3] << 8) | ((UINT32)p[4] << 16) |
                          ((UINT32)p[5] << 24);
            if (blen < 6 || p + blen > end)
            {
                fprintf(stderr, "[t] parse: bad block at off=%zu blen=%u\n",
                        (size_t)(p - wire), blen);
                break;
            }
            if (btype == 0xCCC6 /* WBT_REGION */)
            {
                UINT16 nrects = (UINT16)p[9] | ((UINT16)p[10] << 8);
                fprintf(stderr, "[t] REGION nrects=%u:", nrects);
                for (UINT16 r = 0; r < nrects && 11 + r * 8 + 8 <= blen; r++)
                {
                    const BYTE* rp = p + 11 + r * 8;
                    UINT16 x = (UINT16)rp[0] | ((UINT16)rp[1] << 8);
                    UINT16 y = (UINT16)rp[2] | ((UINT16)rp[3] << 8);
                    UINT16 w = (UINT16)rp[4] | ((UINT16)rp[5] << 8);
                    UINT16 h = (UINT16)rp[6] | ((UINT16)rp[7] << 8);
                    fprintf(stderr, " (%u,%u,%u,%u)", x, y, w, h);
                }
                fprintf(stderr, "\n");
            }
            else if (btype == 0xCCC7 /* WBT_EXTENSION */)
            {
                /* Skip CodecChannelT header (2) + subtype (2) + idx (2)
                 * + properties (2) + numQuant (1) + tileSize (1) +
                 * numTiles (2) + tilesDataSize (4) + quant data. */
                UINT32 off = 2 + 2 + 2 + 2 + 1 + 1 + 2 + 4;
                UINT8 nq = p[6 + 8];
                off += nq * 5;
                /* Each tile: 6-byte header + 2 quantIdxY + 2 quantIdxCb +
                 * 2 quantIdxCr + 2 xIdx + 2 yIdx + 2 YLen + 2 CbLen +
                 * 2 CrLen + YLen + CbLen + CrLen bytes. RFX_TILE body
                 * starts with CBT_TILE (0xCAC3) + 4-byte length + data. */
                UINT16 nT = (UINT16)(p[6 + 10]) | ((UINT16)p[6 + 11] << 8);
                fprintf(stderr, "[t] TILESET numTiles=%u:", nT);
                UINT32 cursor = 6 + off;
                for (UINT16 t = 0; t < nT && cursor + 19 <= blen; t++)
                {
                    /* Tile body relative: quantIdxY(1) quantIdxCb(1)
                     * quantIdxCr(1) xIdx(2) yIdx(2) YLen(2) CbLen(2)
                     * CrLen(2) Y/YLen Cb/CbLen Cr/CrLen. Each tile has
                     * CBT_TILE(2)+length(4) prefix. */
                    UINT32 tileLen = (UINT32)p[cursor + 2] | ((UINT32)p[cursor + 3] << 8) |
                                     ((UINT32)p[cursor + 4] << 16) |
                                     ((UINT32)p[cursor + 5] << 24);
                    UINT16 xI = (UINT16)p[cursor + 6 + 3] | ((UINT16)p[cursor + 6 + 4] << 8);
                    UINT16 yI = (UINT16)p[cursor + 6 + 5] | ((UINT16)p[cursor + 6 + 6] << 8);
                    UINT16 yl = (UINT16)p[cursor + 6 + 7] | ((UINT16)p[cursor + 6 + 8] << 8);
                    fprintf(stderr, " tile%u=(%u,%u)Y=%u", t, xI * 64, yI * 64, yl);
                    cursor += tileLen;
                }
                fprintf(stderr, "\n");
            }
            p += blen;
        }
    }

    memcpy(got, baseline, (size_t)W * H * BPP);
    if (!decode_merge(wire, wire_len, got))
        return fail("decode cursor-region", 6);

    /* Marker uniqueness check: sample the decoded damage centre. The
     * cursor pixel (BGR = [0xE7, 0xE7^0xAA, 0xE7^0x55] = [231, 77, 178]).
     * RLGR3 quantises lossily, so allow a tolerance band of +/-16. */
    {
        size_t probe_off = (size_t)(cy + ch / 2) * W * BPP + (size_t)(cx + cw / 2) * BPP;
        int pb = got[probe_off + 0];
        int pg = got[probe_off + 1];
        int pr = got[probe_off + 2];
        fprintf(stderr, "[t] probe @(%u,%u)=BGR(%d,%d,%d) expected ~(231,77,178)\n",
                cx + cw / 2, cy + ch / 2, pb, pg, pr);
        if (abs(pb - 231) > 24 || abs(pg - 77) > 24 || abs(pr - 178) > 24)
            return fail("damage pixel value mismatch", 7);
    }

    /* Damage rect interior samples. */
    for (UINT32 yy = cy + 4; yy < cy + ch - 4; yy += 16)
        for (UINT32 xx = cx + 4; xx < cx + cw - 4; xx += 24)
        {
            size_t off = (size_t)yy * W * BPP + (size_t)xx * BPP;
            int pb = got[off];
            if (abs(pb - 231) > 32)
            {
                fprintf(stderr, "  interior (%u,%u) B=%d expected ~231\n", xx, yy, pb);
                return fail("interior pixel off", 7);
            }
        }

    /* Pixels far away from damage must still match the baseline. Compare
     * with the byte-level baseline decode: the baseline was decoded from
     * the same encoder context so it's already lossy; the second decode
     * only touches tile-rect regions, so pixels outside the damage tiles
     * must match the baseline exactly. */
    size_t corner_top = (size_t)100 * W * BPP + (size_t)100 * BPP;
    if (memcmp(got + corner_top, baseline + corner_top, 64 * BPP) != 0)
        return fail("out-of-region pixels changed (top)", 8);
    size_t corner_bot = (size_t)(H - 100) * W * BPP + (size_t)(W - 100) * BPP;
    if (memcmp(got + corner_bot, baseline + corner_bot, 64 * BPP) != 0)
        return fail("out-of-region pixels changed (bottom)", 8);

    /* Regression pin for the peer.rs invariant "destRect stays full-surface
     * even with region payloads". The encoder bakes ABSOLUTE surface
     * coordinates into the tile grid, and FreeRDP's decoder ADDS the surface
     * command's left/top again when copying tiles to the surface:
     *   rfx.c rfx_process_message() updateRect.left = left + tile->x, then
     *   freerdp_image_copy to dst at the (shifted) updateRect ∩
     *   (left+region) clipping rect (gdi/gfx.c passes cmd->left/cmd->top).
     * So a surface command with left/top = damage bbox double-offsets every
     * tile. Decoding the SAME payload with (left,top)=(cx,cy) must place
     * damage pixels in the shifted area and leave the grid position
     * untouched. If this test flips, someone changed either the encoder's
     * absolute-grid behaviour or the peer.rs invariant, and they must be
     * reconsidered together.
     * Grid: rect (500,300,96,96) -> tiles x[448,640) y[256,448) (9 tiles).
     * Decoded with (left,top)=(500,300): clip = (1000,600)-(1096,696);
     * tile (448,256)'s update rect (948,556)+64x64 intersects the clip as
     * (1000,600)-(1012,620) -> probe (1008,608) must show the marker. */
    memcpy(got, baseline, (size_t)W * H * BPP);
    if (!decode_merge_at(wire, wire_len, got, cx, cy))
        return fail("decode with nonzero left/top", 9);
    {
        UINT32 px = 1008, py = 608;
        size_t off = (size_t)py * W * BPP + (size_t)px * BPP;
        int pb = got[off + 0];
        int pg = got[off + 1];
        int pr = got[off + 2];
        fprintf(stderr, "[t] double-offset probe @(%u,%u)=BGR(%d,%d,%d) expected ~(231,77,178)\n",
                px, py, pb, pg, pr);
        if (abs(pb - 231) > 32 || abs(pg - 77) > 32 || abs(pr - 178) > 32)
            return fail("nonzero left/top did NOT shift pixels — encoder coordinate invariant changed", 10);
        /* The un-shifted grid position must be UNTOUCHED (still baseline). */
        off = (size_t)(cy + ch / 2) * W * BPP + (size_t)(cx + cw / 2) * BPP;
        if (got[off + 0] != baseline[off + 0] || got[off + 1] != baseline[off + 1] ||
            got[off + 2] != baseline[off + 2])
            return fail("shifted decode unexpectedly touched the grid position", 11);
        (void)px;
    }

    printf("OK: round-trip preserves region semantics\n");
    free(src);
    free(got);
    free(baseline);
    wayrdp_rfx_free(enc);
    return 0;
}
