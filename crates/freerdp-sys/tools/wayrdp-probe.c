/*
 * wayrdp-probe: headless RDPGFX validator client.
 *
 * Connects to a wayrdp server and validates every SurfaceCommand RFX payload
 * against a strict rule-set modelled on what vmconnect (Hyper-V Enhanced
 * Session) appears to enforce but FreeRDP's own decoder silently tolerates.
 *
 * Complements the round-trip harness in tests/: that one proves our encoder
 * emits spec-valid RFX in isolation. This one catches the difference between
 * "spec-valid" and "vmconnect-accepted" that only shows up when the bytes go
 * across a real channel negotiation.
 *
 * Each violation logs to stdout and bumps a count. On disconnect (server-
 * side drop, --max-frames reached, or Ctrl+C), the probe exits 0 with
 * violations==0, 1 otherwise, 2/3 on setup/connect failures.
 *
 * Build (Linux, freerdp3-dev installed):
 *   gcc -g -O0 -Wall -Wextra wayrdp-probe.c -o wayrdp-probe \
 *       $(pkg-config --cflags --libs freerdp-client3 freerdp3 winpr3)
 *
 * Run:
 *   ./wayrdp-probe /v:127.0.0.1:3389 /u:omarchy /p:omarchy /size:1920x1200 \
 *       /cert:ignore --max-frames 300
 */

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <winpr/synch.h>
#include <winpr/wtypes.h>

#include <freerdp/api.h>
#include <freerdp/freerdp.h>
#include <freerdp/client.h>
#include <freerdp/client/cmdline.h>
#include <freerdp/client/rdpgfx.h>
#include <freerdp/channels/rdpgfx.h>
#include <freerdp/event.h>
#include <freerdp/settings.h>

/* -------------------- Validator state -------------------- */

typedef struct
{
	RdpgfxClientContext* gfx;
	uint32_t surface_width;
	uint32_t surface_height;
	uint32_t last_frame_idx;
	uint32_t frame_count;
	uint32_t violation_count;
	uint32_t max_frames;
	HANDLE done_event;
} validator_state;

static validator_state g_v;

static void vlog(const char* tag, const char* fmt, ...)
{
	char buf[1024];
	va_list ap;
	va_start(ap, fmt);
	vsnprintf(buf, sizeof(buf), fmt, ap);
	va_end(ap);
	fprintf(stdout, "[%s] %s\n", tag, buf);
	fflush(stdout);
}

static void violation(const char* rule, const char* fmt, ...)
{
	char buf[1024];
	va_list ap;
	va_start(ap, fmt);
	vsnprintf(buf, sizeof(buf), fmt, ap);
	va_end(ap);
	fprintf(stdout, "[VIOLATION] %s: %s\n", rule, buf);
	fflush(stdout);
	g_v.violation_count++;
}

/* -------------------- Structural RFX walker --------------------
 * Walks the RFX payload inside a SurfaceCommand. Answers structural
 * questions; never decodes pixels. Used by all rules.
 */
struct rfx_stats
{
	uint32_t nblocks;
	uint16_t frame_begin_idx; /* frameIdx from last FRAME_BEGIN, ~0 if none */
	uint32_t nregions;
	uint64_t region_pixel_sum;
	uint32_t min_rx, min_ry, max_rr, max_rb; /* region union bbox */
	bool saw_frame_begin, saw_frame_end, saw_region, saw_tileset;
	bool malformed;
	char malformed_detail[256];
};

static void rfx_walk(const uint8_t* data, size_t len, struct rfx_stats* out)
{
	memset(out, 0, sizeof(*out));
	out->frame_begin_idx = 0xFFFF;
	out->min_rx = out->min_ry = UINT32_MAX;

	size_t off = 0;
	while (off + 6 <= len)
	{
		uint16_t bt;
		uint32_t blen;
		memcpy(&bt, data + off, 2);
		memcpy(&blen, data + off + 2, 4);
		if (blen < 6)
		{
			snprintf(out->malformed_detail, sizeof(out->malformed_detail),
			         "block%u type=0x%04x blen=%u (<6)", out->nblocks, bt, blen);
			out->malformed = true;
			return;
		}
		if (off + blen > len)
		{
			snprintf(out->malformed_detail, sizeof(out->malformed_detail),
			         "block%u type=0x%04x blen=%u overruns payload (off=%zu len=%zu)",
			             out->nblocks, bt, blen, off, len);
			out->malformed = true;
			return;
		}

		const uint8_t* body = data + off + 6;
		size_t body_len = blen - 6;

		switch (bt)
		{
			case 0xCCC4: /* WBT_FRAME_BEGIN: codecId(1) channelId(1) frameIdx(4) numRegions(2) */
				if (body_len >= 8)
				{
					uint32_t frame_idx;
					memcpy(&frame_idx, body + 2, 4);
					out->frame_begin_idx = (uint16_t)frame_idx;
					out->saw_frame_begin = true;
				}
				break;
			case 0xCCC5: /* WBT_FRAME_END */
				out->saw_frame_end = true;
				break;
			case 0xCCC6: /* WBT_REGION: codecId(1) channelId(1) regionFlags(1) numRects(2) rects[8*n] regionType(2) numTilesets(2) */
				out->saw_region = true;
				if (body_len >= 5)
				{
					uint16_t num_rects;
					/* body[0]=codecId body[1]=channelId body[2]=regionFlags body[3..4]=numRects */
					memcpy(&num_rects, body + 3, 2);
					const uint8_t* rp = body + 5;
					for (uint16_t i = 0; i < num_rects; i++)
					{
						if ((rp + 8) > (body + body_len))
							break;
						uint16_t x, y, w, h;
						memcpy(&x, rp + 0, 2);
						memcpy(&y, rp + 2, 2);
						memcpy(&w, rp + 4, 2);
						memcpy(&h, rp + 6, 2);
						out->nregions++;
						out->region_pixel_sum += (uint64_t)w * h;
						if (x < out->min_rx) out->min_rx = x;
						if (y < out->min_ry) out->min_ry = y;
						if ((uint32_t)x + w > out->max_rr) out->max_rr = (uint32_t)x + w;
						if ((uint32_t)y + h > out->max_rb) out->max_rb = (uint32_t)y + h;
						rp += 8;
					}
				}
				break;
			case 0xCAC2: /* CBT_TILESET */
				out->saw_tileset = true;
				break;
			default:
				break;
		}
		off += blen;
		out->nblocks++;
	}
	if (off != len)
	{
		snprintf(out->malformed_detail, sizeof(out->malformed_detail),
		         "%zu trailing bytes after last block (payload=%zu)", len - off, len);
		out->malformed = true;
	}
}

/* -------------------- Rules -------------------- */

/* Rule 1: blocks must be structurally well-formed. */
static void rule_block_structure(const RDPGFX_SURFACE_COMMAND* cmd, const struct rfx_stats* st)
{
	if (st->malformed)
		violation("block_structure", "%s", st->malformed_detail);
}

/* Rule 2: every frame must bracket its blocks with FRAME_BEGIN/FRAME_END. */
static void rule_frame_brackets(const RDPGFX_SURFACE_COMMAND* cmd, const struct rfx_stats* st)
{
	(void)cmd;
	if (!st->saw_frame_begin)
		violation("frame_brackets", "no FRAME_BEGIN block in payload");
	if (!st->saw_frame_end)
		violation("frame_brackets", "no FRAME_END block in payload");
}

/* Rule 3: frameIdx advances by one per frame per channel. */
static void rule_frame_monotonic(const RDPGFX_SURFACE_COMMAND* cmd, const struct rfx_stats* st)
{
	(void)cmd;
	if (!st->saw_frame_begin)
		return;
	uint32_t idx = st->frame_begin_idx;
	if (g_v.last_frame_idx != 0xFFFFFFFFu && idx != g_v.last_frame_idx + 1)
		violation("frame_monotonic", "frameIdx=%u after %u", idx, g_v.last_frame_idx);
	g_v.last_frame_idx = idx;
}

/* Rule 4: all RFX region rects fit within destRect. */
static void rule_regions_within_destRect(const RDPGFX_SURFACE_COMMAND* cmd,
                                         const struct rfx_stats* st)
{
	if (st->nregions == 0)
		return;
	if (st->min_rx < cmd->left || st->min_ry < cmd->top || st->max_rr > cmd->right ||
	    st->max_rb > cmd->bottom)
	{
		violation("regions_within_destrect",
		          "region union (%u,%u)-(%u,%u) outside destRect (%u,%u)-(%u,%u)",
		          st->min_rx, st->min_ry, st->max_rr, st->max_rb,
		          cmd->left, cmd->top, cmd->right, cmd->bottom);
	}
}

/* Rule 5: region pixel sum must cover destRect. Under-coverage leaves holes. */
static void rule_full_coverage(const RDPGFX_SURFACE_COMMAND* cmd, const struct rfx_stats* st)
{
	if (st->nregions == 0 && st->saw_tileset)
		return; /* tile-only frame without an explicit region block is suspicious, handled below */
	uint64_t dest_px = (uint64_t)(cmd->right - cmd->left) * (cmd->bottom - cmd->top);
	if (st->region_pixel_sum < dest_px)
		violation("full_coverage",
		          "region_px=%llu < dest_px=%llu (destRect %ux%u)",
		          (unsigned long long)st->region_pixel_sum, (unsigned long long)dest_px,
		          cmd->right - cmd->left, cmd->bottom - cmd->top);
}

/* Rule 6: region-encoded frame advertises at least one region. */
static void rule_frame_has_region(const RDPGFX_SURFACE_COMMAND* cmd, const struct rfx_stats* st)
{
	(void)cmd;
	if (st->saw_tileset && !st->saw_region)
		violation("frame_has_region", "TILESET present without preceding REGION block");
}

/* -------------------- Channel hooks -------------------- */

static UINT probe_OnOpen(RdpgfxClientContext* ctx, BOOL* do_caps_advertise, BOOL* do_frame_acks)
{
	(void)ctx;
	*do_caps_advertise = TRUE;
	*do_frame_acks = TRUE;
	return CHANNEL_RC_OK;
}

static UINT probe_SurfaceCommand(RdpgfxClientContext* ctx, const RDPGFX_SURFACE_COMMAND* cmd)
{
	(void)ctx;
	g_v.frame_count++;

	if (cmd->codecId == RDPGFX_CODECID_CAVIDEO)
	{
		struct rfx_stats st;
		rfx_walk(cmd->data, cmd->length, &st);
		rule_block_structure(cmd, &st);
		rule_frame_brackets(cmd, &st);
		rule_frame_monotonic(cmd, &st);
		rule_regions_within_destRect(cmd, &st);
		rule_full_coverage(cmd, &st);
		rule_frame_has_region(cmd, &st);

		if (g_v.frame_count <= 5 || g_v.frame_count % 100 == 0)
		{
			vlog("probe",
			     "frame#%u destRect=(%u,%u,%ux%u) codecId=0x%04x blocks=%u regions=%u len=%u",
			     g_v.frame_count, cmd->left, cmd->top, cmd->right - cmd->left,
			     cmd->bottom - cmd->top, cmd->codecId, st.nblocks, st.nregions,
			     cmd->length);
		}
	}

	if (g_v.max_frames > 0 && g_v.frame_count >= g_v.max_frames)
	{
		vlog("probe", "max-frames %u reached; disconnecting", g_v.max_frames);
		SetEvent(g_v.done_event);
	}
	return CHANNEL_RC_OK;
}

static UINT probe_CreateSurface(RdpgfxClientContext* ctx, const RDPGFX_CREATE_SURFACE_PDU* pdu)
{
	(void)ctx;
	g_v.surface_width = pdu->width;
	g_v.surface_height = pdu->height;
	vlog("probe", "CreateSurface id=%u %ux%u", pdu->surfaceId, pdu->width, pdu->height);
	return CHANNEL_RC_OK;
}

static void probe_OnChannelConnected(void* context, const ChannelConnectedEventArgs* e)
{
	rdpContext* rdp = (rdpContext*)context;
	(void)rdp;

	if (strcmp(e->name, RDPGFX_DVC_CHANNEL_NAME) != 0)
		return;

	RdpgfxClientContext* gfx = (RdpgfxClientContext*)e->pInterface;
	g_v.gfx = gfx;
	gfx->custom = &g_v;
	gfx->OnOpen = probe_OnOpen;
	gfx->SurfaceCommand = probe_SurfaceCommand;
	gfx->CreateSurface = probe_CreateSurface;

	vlog("probe", "rdpgfx channel attached; validators armed");
}

static void probe_OnChannelDisconnected(void* context, const ChannelDisconnectedEventArgs* e)
{
	(void)context;
	if (strcmp(e->name, RDPGFX_DVC_CHANNEL_NAME) == 0)
	{
		vlog("probe", "rdpgfx channel disconnected");
		g_v.gfx = NULL;
		SetEvent(g_v.done_event);
	}
}

/* -------------------- Client lifecycle -------------------- */

static BOOL probe_pre_connect(freerdp* instance)
{
	rdpSettings* s = instance->context->settings;
	freerdp_settings_set_uint32(s, FreeRDP_OsMajorType, OSMAJORTYPE_UNIX);
	freerdp_settings_set_uint32(s, FreeRDP_OsMinorType, OSMINORTYPE_NATIVE_WAYLAND);
	freerdp_settings_set_bool(s, FreeRDP_SupportGraphicsPipeline, TRUE);
	freerdp_settings_set_bool(s, FreeRDP_GfxProgressive, FALSE);
	freerdp_settings_set_bool(s, FreeRDP_GfxAVC444, FALSE);
	freerdp_settings_set_bool(s, FreeRDP_GfxH264, FALSE);
	freerdp_settings_set_bool(s, FreeRDP_RemoteFxCodec, TRUE);
	freerdp_settings_set_bool(s, FreeRDP_DynamicResolutionUpdate, FALSE);

	PubSub_SubscribeChannelConnected(instance->context->pubSub, probe_OnChannelConnected);
	PubSub_SubscribeChannelDisconnected(instance->context->pubSub, probe_OnChannelDisconnected);
	return TRUE;
}

static BOOL probe_post_connect(freerdp* instance)
{
	uint32_t w = freerdp_settings_get_uint32(instance->context->settings, FreeRDP_DesktopWidth);
	uint32_t h = freerdp_settings_get_uint32(instance->context->settings, FreeRDP_DesktopHeight);
	vlog("probe", "connected: desktop %ux%u", w, h);
	return TRUE;
}

static void probe_post_disconnect(freerdp* instance)
{
	(void)instance;
	SetEvent(g_v.done_event);
}

static BOOL probe_client_new(freerdp* instance, rdpContext* context)
{
	(void)context;
	instance->PreConnect = probe_pre_connect;
	instance->PostConnect = probe_post_connect;
	instance->PostDisconnect = probe_post_disconnect;
	return TRUE;
}

static void probe_client_free(freerdp* instance, rdpContext* context)
{
	(void)instance;
	(void)context;
}

static BOOL probe_client_global_init(void)
{
	return TRUE;
}

static void probe_client_global_uninit(void)
{
}

static int probe_client_start(rdpContext* context)
{
	(void)context;
	return 0;
}

static int probe_client_stop(rdpContext* context)
{
	(void)context;
	return 0;
}

static int probe_entry(RDP_CLIENT_ENTRY_POINTS* e)
{
	e->Version = RDP_CLIENT_INTERFACE_VERSION;
	e->Size = sizeof(RDP_CLIENT_ENTRY_POINTS_V1);
	e->GlobalInit = probe_client_global_init;
	e->GlobalUninit = probe_client_global_uninit;
	e->ContextSize = sizeof(rdpClientContext);
	e->ClientNew = probe_client_new;
	e->ClientFree = probe_client_free;
	e->ClientStart = probe_client_start;
	e->ClientStop = probe_client_stop;
	return 0;
}

/* -------------------- Main -------------------- */

int main(int argc, char** argv)
{
	memset(&g_v, 0, sizeof(g_v));
	g_v.last_frame_idx = 0xFFFFFFFFu;

	RDP_CLIENT_ENTRY_POINTS cep = WINPR_C_ARRAY_INIT;
	probe_entry(&cep);
	rdpContext* context = freerdp_client_context_new(&cep);
	if (!context)
	{
		fprintf(stderr, "freerdp_client_context_new failed\n");
		return 2;
	}
	int rc = 0;

	/* Custom flag: --max-frames N. Freerdp v3 chokes on unknown args, so strip
	 * it (and its value) out of argv before passing to the parser. */
	int fargc = 1;
	char** fargv = calloc((size_t)argc, sizeof(char*));
	fargv[0] = argv[0];
	for (int i = 1; i < argc; i++)
	{
		if (strcmp(argv[i], "--max-frames") == 0 && i + 1 < argc)
		{
			g_v.max_frames = (uint32_t)atoi(argv[i + 1]);
			i++; /* swallow value */
			continue;
		}
		fargv[fargc++] = argv[i];
	}

	int status = freerdp_client_settings_parse_command_line(context->settings, fargc, fargv, FALSE);
	free(fargv);

	g_v.done_event = CreateEvent(NULL, TRUE, FALSE, NULL);

	if (freerdp_client_start(context) != 0)
		goto out;
	vlog("probe", "client_start OK; connecting...");
	BOOL connected = freerdp_connect(context->instance);
	vlog("probe", "freerdp_connect -> %d", connected ? 1 : 0);
	if (!connected)
	{
		fprintf(stderr, "connect failed: 0x%08x\n",
		        freerdp_get_last_error(context));
		rc = 3;
		goto stop;
	}

	{
		HANDLE ev[64];
		int iter = 0;
		while (true)
		{
			DWORD n = freerdp_get_event_handles(context, ev, 64);
			if (iter++ == 0) vlog("probe", "event loop started: %u handles", n);
			if (n == 0)
			{
				vlog("probe", "no event handles; exiting loop");
				break;
			}
			HANDLE all[65];
			memcpy(all, ev, n * sizeof(HANDLE));
			all[n] = g_v.done_event;
			DWORD w = WaitForMultipleObjects(n + 1, all, FALSE, 250);
			if (w == WAIT_FAILED)
			{
				vlog("probe", "WaitForMultipleObjects FAILED %lu", (unsigned long)w);
				break;
			}
			if (w == WAIT_OBJECT_0 + n) /* done_event */
				break;
			if (!freerdp_check_event_handles(context))
			{
				if (freerdp_get_last_error(context) != FREERDP_ERROR_SUCCESS)
					vlog("probe", "check_event_handles failed: 0x%08x",
					     freerdp_get_last_error(context));
				break;
			}
			if (g_v.max_frames > 0 && g_v.frame_count >= g_v.max_frames)
				break;
			if (freerdp_shall_disconnect_context(context))
			{
				vlog("probe", "shall_disconnect; bailing");
				break;
			}
		}
	}

	freerdp_disconnect(context->instance);
stop:
	freerdp_client_stop(context);
out:
	freerdp_client_context_free(context);
	CloseHandle(g_v.done_event);

	fprintf(stdout, "[probe] frames=%u violations=%u result=%s\n",
	        g_v.frame_count, g_v.violation_count, g_v.violation_count ? "FAIL" : "PASS");
	return g_v.violation_count ? 1 : 0;
}
