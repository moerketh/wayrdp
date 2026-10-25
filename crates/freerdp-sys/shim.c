/*
 * wayrdp freerdp-sys shim.
 *
 * FreeRDP 3 headers expose some functionality only via macros, static-inline
 * helpers, and WINPR calling-convention wrappers that bindgen cannot see.
 * Every such case gets a `wayrdp_*` wrapper here — D1-2 of Phase 0 grows this
 * file while wiring the listener.
 */

#include <stdio.h>
#include <stdlib.h>

#include <winpr/wtypes.h>
#include <winpr/synch.h>
#include <winpr/stream.h>
#include <winpr/wtsapi.h>
#include <freerdp/freerdp.h>
#include <freerdp/listener.h>
#include <freerdp/peer.h>
#include <freerdp/update.h>
#include <freerdp/input.h>
#include <freerdp/settings.h>
#include <freerdp/channels/channels.h>
#include <freerdp/channels/drdynvc.h>
#include <freerdp/channels/rdpgfx.h>
#include <freerdp/channels/disp.h>
#include <freerdp/channels/wtsvc.h>
#include <freerdp/server/rdpgfx.h>
#include <freerdp/server/disp.h>
#include <freerdp/channels/cliprdr.h>
#include <freerdp/server/cliprdr.h>
#include <freerdp/codec/rfx.h>
#include <freerdp/codec/color.h>
#include <winpr/input.h>

/* WinPR wait/event helpers. These live in <winpr/synch.h> but are not under
 * a `freerdp_*` allowlist prefix, so bindgen drops them. The peer event loop
 * (D3) needs them. Return types follow winpr: DWORD wait result, the raw
 * HANDLE pointer. */

/* Wait on a single handle; returns WAIT_OBJECT_0 (0) on signal, WAIT_FAILED
 * (0xFFFFFFFF) on error. Wraps WaitForSingleObject. */
DWORD wayrdp_wait_single(HANDLE handle, DWORD milliseconds)
{
	return WaitForSingleObject(handle, milliseconds);
}

/* Wait on the first `count` handles in `handles`; returns WAIT_OBJECT_0 + index
 * of the signalled handle, or WAIT_FAILED. bWaitAll is FALSE (any). */
DWORD wayrdp_wait_multiple(DWORD count, const HANDLE* handles, DWORD milliseconds)
{
	return WaitForMultipleObjects(count, handles, FALSE, milliseconds);
}

/* Create a manual-reset, initially-non-signalled event (for the D3 frame
 * trigger). Returns NULL on failure. */
HANDLE wayrdp_create_event(void)
{
	return CreateEvent(NULL, TRUE, FALSE, NULL);
}

BOOL wayrdp_set_event(HANDLE handle)
{
	return SetEvent(handle);
}

BOOL wayrdp_reset_event(HANDLE handle)
{
	return ResetEvent(handle);
}

BOOL wayrdp_close_handle(HANDLE handle)
{
	return CloseHandle(handle);
}

/* Protocol + sync constants that the C headers define as macros. bindgen
 * cannot see `#define`s, so expose them as functions. */

UINT32 wayrdp_cmdtype_set_surface_bits(void) { return CMDTYPE_SET_SURFACE_BITS; }
UINT32 wayrdp_cmdtype_stream_surface_bits(void) { return CMDTYPE_STREAM_SURFACE_BITS; }
UINT32 wayrdp_cmdtype_frame_marker(void) { return CMDTYPE_FRAME_MARKER; }
UINT32 wayrdp_frame_action_begin(void) { return SURFACECMD_FRAMEACTION_BEGIN; }
UINT32 wayrdp_frame_action_end(void) { return SURFACECMD_FRAMEACTION_END; }
UINT32 wayrdp_encryption_level_client_compatible(void) { return ENCRYPTION_LEVEL_CLIENT_COMPATIBLE; }
/* RDP-security-without-encryption path for vsock/vmconnect (see peer.rs):
   advertise only "no encryption" so FreeRDP skips client-random session key
   establishment and never touches its (missing on some builds) RC4/MD4. */
UINT32 wayrdp_encryption_level_none(void) { return ENCRYPTION_LEVEL_NONE; }
UINT32 wayrdp_encryption_method_none(void) { return ENCRYPTION_METHOD_NONE; }
UINT32 wayrdp_settings_key_uint32_encryption_methods(void) {
    return FreeRDP_EncryptionMethods;
}
/* Compute-macro pixel format (FREERDP_PIXEL_FORMAT(...)) for the gfx surface
 * format fields; #define-computed so bindgen cannot see the value. */
UINT32 wayrdp_pixel_format_bgrx32(void) { return PIXEL_FORMAT_BGRX32; }
/* Wire pixel format for RDPGFX_CREATE_SURFACE_PDU.pixelFormat (MS-RDPEGFX
 * 2.2.2.16) — NOT the FreeRDP-internal PIXEL_FORMAT_* the surface command
 * uses; the CreateSurface serializer writes the byte as-is. */
UINT8 wayrdp_gfx_pixel_format_xrgb_8888(void) { return GFX_PIXEL_FORMAT_XRGB_8888; }
DWORD wayrdp_wait_object_0(void) { return WAIT_OBJECT_0; }
DWORD wayrdp_wait_failed(void) { return WAIT_FAILED; }
DWORD wayrdp_infinite(void) { return INFINITE; }

/* Virtual Channel Manager (static channels incl. drdynvc). Declared in
 * <freerdp/channels/wtsvc.h>; the WTS* prefix is covered by the bindgen
 * allowlist but the typedef'd function pointers make the generated bindings
 * unusable directly, so wrap them. */
BOOL wayrdp_vcm_open(HANDLE vcm) { return WTSVirtualChannelManagerOpen(vcm); }
/* Drain the outbound queue only (autoOpen=FALSE). Opening auto-sends the
 * drdynvc capabilities request, but WTSVirtualChannelManagerOpen flips the
 * state to INITIALIZED *before* attempting the open — if the client hasn't
 * joined drdynvc yet, the request is silently dropped with no retry path,
 * which is exactly what happened when every drain auto-opened. The mainloop
 * calls wayrdp_vcm_open explicitly once IsChannelJoined reports TRUE. */
BOOL wayrdp_vcm_check_fd(HANDLE vcm) { return WTSVirtualChannelManagerCheckFileDescriptorEx(vcm, FALSE); }
HANDLE wayrdp_vcm_get_event(HANDLE vcm) { return WTSVirtualChannelManagerGetEventHandle(vcm); }

/* Per-connection server handle that owns the VCM (wtsapi; WINAPI calling
 * convention hides it from bindgen). `name` must be the rdpContext pointer —
 * FreeRDP's winpr server mode keys the per-context channel table on it
 * (sfreerdp passes (LPSTR)client->context). */
HANDLE wayrdp_wts_open_server(void* name) { return WTSOpenServerA((LPSTR)name); }

/* drdynvc plumbing for the deferred rdpgfx Open (D4): shadow only opens
 * rdpgfx once the client joined the drdynvc static channel and the manager
 * reports DRDYNVC_STATE_READY. The state enum and channel-name string are
 * macros in wtsvc.h, invisible to bindgen. */
BYTE wayrdp_drdynvc_state_ready(void) { return DRDYNVC_STATE_READY; }
const char* wayrdp_drdynvc_channel_name(void) { return DRDYNVC_SVC_CHANNEL_NAME; }
BOOL wayrdp_vcm_is_channel_joined(HANDLE vcm, const char* name)
{
	return WTSVirtualChannelManagerIsChannelJoined(vcm, name);
}
BYTE wayrdp_vcm_drdynvc_state(HANDLE vcm)
{
	return WTSVirtualChannelManagerGetDrdynvcState(vcm);
}
void wayrdp_wts_close_server(HANDLE server) { WTSCloseServer(server); }

/* One-time startup: install FreeRDP's built-in server-side WTS API table
 * (libfreerdp/core/channels.c). Without this the WTS* calls route to the
 * FreeRDS socket stubs and WTSOpenServerA returns NULL on Linux. Call before
 * creating the listener. */
BOOL wayrdp_register_wts_api(void)
{
	return WTSRegisterWtsApiFunctionTable(FreeRDP_InitWtsApi());
}

/* D4: RDPGFX (EGFX) server context plumbing.
 *
 * The RdpgfxServerContext struct is bindgen-visible, but three things still
 * need C-side help: function-pointer invocation of the context's own ops
 * table (bindgen emits them as Option<extern fn>, callable, but the call
 * chain is ugly and error-prone in Rust), the rdpContext field assignment
 * (field type is server-side rdpContext*), and RemoteFX encoding, which is
 * wStream/RFX_CONTEXT based — both intentionally not surfaced through
 * bindgen. */

void wayrdp_rdpgfx_set_rdpcontext(RdpgfxServerContext* c, void* ctx)
{
	c->rdpcontext = (rdpContext*)ctx;
}

UINT wayrdp_rdpgfx_call_initialize(RdpgfxServerContext* c, BOOL external_thread)
{
	return c->Initialize(c, external_thread);
}
BOOL wayrdp_rdpgfx_call_open(RdpgfxServerContext* c) { return c->Open(c); }
BOOL wayrdp_rdpgfx_call_close(RdpgfxServerContext* c) { return c->Close(c); }

UINT wayrdp_rdpgfx_call_surface_frame_command(RdpgfxServerContext* c,
                                              const RDPGFX_SURFACE_COMMAND* cmd,
                                              const RDPGFX_START_FRAME_PDU* start,
                                              const RDPGFX_END_FRAME_PDU* end)
{
	/* Wire-dump toggle: WAYRDP_DUMP_GFX=<dir> writes one blob per frame
	 * capturing every field of the STARTFRAME / WIRETOSURFACE_1 /
	 * ENDFRAME triple exactly as we're about to hand it to FreeRDP's
	 * rdpgfx_server_packet_send. Layout is the raw marshalled form of
	 * rdpgfx_write_surface_command (no PDU headers), so byte-diffing two
	 * runs highlights any structural difference between a full-frame
	 * encode and a region-encoded one. Filenames use the 8-digit frameId
	 * for stable ordering. */
	static const char* dump_dir = NULL;
	static int dump_init = 0;
	if (!dump_init)
	{
		dump_dir = getenv("WAYRDP_DUMP_GFX");
		dump_init = 1;
	}
	if (dump_dir && dump_dir[0] && start)
	{
		char path[512];
		snprintf(path, sizeof(path), "%s/frame-%08u.bin", dump_dir, start->frameId);
		FILE* f = fopen(path, "wb");
		if (f)
		{
			/* STARTFRAME pdu body: timestamp + frameId. */
			fwrite(&start->timestamp, 4, 1, f);
			fwrite(&start->frameId, 4, 1, f);
			/* WIRETOSURFACE_1 body: surfaceId, codecId, format, rect,
			 * bitmapDataLength, then bitmapData. */
			UINT16 surfaceId = (UINT16)cmd->surfaceId;
			UINT16 codecId = (UINT16)cmd->codecId;
			UINT8 pfmt = (cmd->format == PIXEL_FORMAT_BGRA32) ? 0x21 : 0x20;
			UINT16 rc_left = (UINT16)cmd->left;
			UINT16 rc_top = (UINT16)cmd->top;
			UINT16 rc_right = (UINT16)cmd->right;
			UINT16 rc_bottom = (UINT16)cmd->bottom;
			UINT32 blen = cmd->length;
			fwrite(&surfaceId, 2, 1, f);
			fwrite(&codecId, 2, 1, f);
			fwrite(&pfmt, 1, 1, f);
			fwrite(&rc_left, 2, 1, f);
			fwrite(&rc_top, 2, 1, f);
			fwrite(&rc_right, 2, 1, f);
			fwrite(&rc_bottom, 2, 1, f);
			fwrite(&blen, 4, 1, f);
			if (blen && cmd->data)
				fwrite(cmd->data, 1, blen, f);
			/* ENDFRAME pdu body: frameId only. */
			fwrite(&end->frameId, 4, 1, f);
			fclose(f);
		}
	}
	return c->SurfaceFrameCommand(c, cmd, start, end);
}

UINT wayrdp_rdpgfx_call_caps_confirm(RdpgfxServerContext* c, RDPGFX_CAPS_CONFIRM_PDU* pdu)
{
	return c->CapsConfirm(c, pdu);
}

UINT wayrdp_rdpgfx_call_create_surface(RdpgfxServerContext* c,
                                       const RDPGFX_CREATE_SURFACE_PDU* pdu)
{
	return c->CreateSurface(c, pdu);
}

UINT wayrdp_rdpgfx_call_delete_surface(RdpgfxServerContext* c, UINT16 surface_id)
{
	RDPGFX_DELETE_SURFACE_PDU pdu = { surface_id };
	return c->DeleteSurface(c, &pdu);
}

UINT wayrdp_rdpgfx_call_map_to_output(RdpgfxServerContext* c, UINT16 surface_id,
                                      UINT32 x, UINT32 y)
{
	RDPGFX_MAP_SURFACE_TO_OUTPUT_PDU pdu = { surface_id, 0, x, y };
	return c->MapSurfaceToOutput(c, &pdu);
}

/* Minimal per-surface RemoteFX encoder. Opaque to Rust; owns its RFX_CONTEXT
 * and one reusable wStream. Shadow uses the full rdpShadowEncoder machinery;
 * Phase 0 only ever encodes one full-surface rect per frame, so a single
 * context + one rect suffices. */
typedef struct s_wayrdp_rfx
{
	RFX_CONTEXT* rfx;
	wStream* s;
	UINT32 width;
	UINT32 height;
} WayrdpRfx;

void wayrdp_rfx_free(WayrdpRfx* e);
BOOL wayrdp_rfx_encode_region(WayrdpRfx* e, const BYTE* src, UINT32 src_step,
                              const UINT16* rects, size_t nrects,
                              const BYTE** out_data, UINT32* out_len);

WayrdpRfx* wayrdp_rfx_new(UINT32 width, UINT32 height)
{
	if (width == 0 || height == 0)
		return NULL;
	WayrdpRfx* e = (WayrdpRfx*)calloc(1, sizeof(WayrdpRfx));
	if (!e)
		return NULL;
	e->width = width;
	e->height = height;
	e->rfx = rfx_context_new(TRUE);
	e->s = Stream_New(NULL, 4096);
	if (!e->rfx || !e->s)
	{
		wayrdp_rfx_free(e);
		return NULL;
	}
	/* rfx_context_new leaves width/height 0; the channels block of every
	 * composed message carries context->width/height, and a 0x0 channel is
	 * rejected by clients ("invalid channel width/height: 0x0"). Shadow
	 * calls rfx_context_reset after creating the context; so must we. */
	if (!rfx_context_reset(e->rfx, width, height))
	{
		wayrdp_rfx_free(e);
		return NULL;
	}
	rfx_context_set_pixel_format(e->rfx, PIXEL_FORMAT_BGRX32);
	/* Previously recreated with THREADING_FLAGS_DISABLE_THREADS because
	 * multi-rect damage lists raced in the tile threadpool. The caller now
	 * always sends a single merged bbox rect (or full-frame), so enable the
	 * threadpool: full-frame 1920x1200 drops from ~18ms to ~5ms per frame. */
	return e;
}

/* Encode `width`x`height` BGRX32 pixels as one RFX message. On success
 * `*out_data`/`*out_len` describe a buffer owned by the encoder — valid
 * until the next wayrdp_rfx_encode or wayrdp_rfx_free call.
 *
 * `rects`/`nrects` carry the compositor-damage list for this frame: the
 * encoder transforms only those regions, so a moving cursor encodes a few
 * 64x64 tiles instead of the whole 1920x1200 surface. Pass `NULL,0` to
 * fall back to a full-frame encode (correct behavior for resize/first-
 * frame, where damage is empty-but-everything-changed). Rects outside the
 * surface are clipped to prevent encoder overruns. */
BOOL wayrdp_rfx_encode(WayrdpRfx* e, const BYTE* src, UINT32 src_step,
                       const BYTE** out_data, UINT32* out_len)
{
	return wayrdp_rfx_encode_region(e, src, src_step, NULL, 0, out_data, out_len);
}

/* Region-aware variant. `rects` is a packed `x,y,w,h` array of u16 quads,
 * exactly `nrects` quadruples. See above for the NULL/0 fallback. */
BOOL wayrdp_rfx_encode_region(WayrdpRfx* e, const BYTE* src, UINT32 src_step,
                              const UINT16* rects, size_t nrects,
                              const BYTE** out_data, UINT32* out_len)
{
	if (!e || !e->rfx || !e->s || !src || !out_data || !out_len)
		return FALSE;
	if (e->width > INT16_MAX || e->height > INT16_MAX)
		return FALSE;

	/* Clamp the rect count to something sane for the PDU. RFX caps the
	 * region list at 65535 entries but practically a moving cursor yields
	 * 1-10 rects; larger bursts come from scroll effects and are still
	 * bounded by compositor tile granularity. */
	if (nrects > 4096)
		nrects = 4096;

	RFX_RECT* rfx_rects = alloca((nrects ? nrects : 1) * sizeof(RFX_RECT));
	UINT32 used = 0;
	if (rects && nrects > 0)
	{
		for (size_t i = 0; i < nrects; i++)
		{
			UINT32 x = rects[i * 4 + 0];
			UINT32 y = rects[i * 4 + 1];
			UINT32 w = rects[i * 4 + 2];
			UINT32 h = rects[i * 4 + 3];
			/* Clip to the surface; ignore fully-out-of-bounds rects. */
			if (x >= e->width || y >= e->height)
				continue;
			if (x + w > e->width)
				w = e->width - x;
			if (y + h > e->height)
				h = e->height - y;
			if (w == 0 || h == 0)
				continue;
			rfx_rects[used].x = (UINT16)x;
			rfx_rects[used].y = (UINT16)y;
			rfx_rects[used].width = (UINT16)w;
			rfx_rects[used].height = (UINT16)h;
			used++;
		}
	}
	if (used == 0)
	{
		/* Fallback: encode the whole frame. Matches the pre-region
		 * behavior so resize/first-frame paths stay correct. */
		rfx_rects[0].x = 0;
		rfx_rects[0].y = 0;
		rfx_rects[0].width = (UINT16)e->width;
		rfx_rects[0].height = (UINT16)e->height;
		used = 1;
	}

	Stream_SetPosition(e->s, 0);
	if (!rfx_compose_message(e->rfx, e->s, rfx_rects, used, src, e->width, e->height, src_step))
		return FALSE;
	*out_data = Stream_Buffer(e->s);
	*out_len = (UINT32)Stream_GetPosition(e->s);
	return TRUE;
}

void wayrdp_rfx_free(WayrdpRfx* e)
{
	if (!e)
		return;
	if (e->rfx)
		rfx_context_free(e->rfx);
	if (e->s)
		Stream_Free(e->s, TRUE);
	free(e);
}

/* MS-RDPEGFX 2.2.2.14 ResetGraphics: tell the client the output geometry so
 * it can (re)allocate its surface bookkeeping. Shadow sends this before the
 * first CreateSurface; a single primary-monitor definition covering the whole
 * desktop matches what a one-monitor enhanced session reports. */
UINT wayrdp_rdpgfx_call_reset_graphics(RdpgfxServerContext* c, UINT32 width, UINT32 height)
{
	MONITOR_DEF mon = { 0 };
	mon.left = 0;
	mon.top = 0;
	mon.right = (INT32)width - 1;
	mon.bottom = (INT32)height - 1;
/* DISPLAYCONTROL_MONITOR_PRIMARY (MS-RDPEGFX 2.2.1.3.1); a #define so
 * bindgen dropped it. */
mon.flags = 0x1;
	RDPGFX_RESET_GRAPHICS_PDU pdu;
	pdu.width = width;
	pdu.height = height;
	pdu.monitorCount = 1;
	pdu.monitorDefArray = &mon;
	return c->ResetGraphics(c, &pdu);
}

/* D5: Display-Control (RDPEDISP) ops. Same bindgen limitation as the rdpgfx
 * ops table: wrap the function-pointer calls. Open spawns the channel's
 * reader thread; Caps advertises our limits so clients send layouts. */
UINT wayrdp_disp_call_open(DispServerContext* c) { return c->Open(c); }
UINT wayrdp_disp_call_caps(DispServerContext* c) { return c->DisplayControlCaps(c); }

/* CLIPRDR (MS-RDPECLIP) server context. A static channel: the client joins
 * it during MCS connect, so it is started from post_connect (shadow does the
 * same). Start() opens the channel and spawns the channel's own reader
 * thread, which sends Capabilities + MonitorReady first
 * (autoInitializationSequence) and then runs the Client* callbacks. The
 * Server* senders below go through the VCM's locked write path and may be
 * called from any thread. Same bindgen limitation as rdpgfx/disp: wrap the
 * ops-table calls. */
const char* wayrdp_cliprdr_channel_name(void) { return CLIPRDR_SVC_CHANNEL_NAME; }

CliprdrServerContext* wayrdp_cliprdr_new(HANDLE vcm, void* rdpctx)
{
	CliprdrServerContext* c = cliprdr_server_context_new(vcm);
	if (!c)
		return NULL;
	c->rdpcontext = (rdpContext*)rdpctx;
	c->useLongFormatNames = TRUE;
	/* File transfer as streams (FileContents) with names only, no paths. */
	c->streamFileClipEnabled = TRUE;
	c->fileClipNoFilePaths = TRUE;
	c->canLockClipData = FALSE;
	c->hasHugeFileSupport = TRUE;
	return c;
}

UINT wayrdp_cliprdr_start(CliprdrServerContext* c) { return c->Start(c); }
UINT wayrdp_cliprdr_stop(CliprdrServerContext* c) { return c->Stop(c); }

UINT wayrdp_cliprdr_send_format_list(CliprdrServerContext* c, const UINT32* ids,
                                     const char* const* names, UINT32 n)
{
	CLIPRDR_FORMAT* formats = calloc(n ? n : 1, sizeof(CLIPRDR_FORMAT));
	if (!formats)
		return ERROR_OUTOFMEMORY;
	for (UINT32 i = 0; i < n; i++)
	{
		formats[i].formatId = ids[i];
		formats[i].formatName = (char*)names[i]; /* NULL for standard formats */
	}
	CLIPRDR_FORMAT_LIST list = { 0 };
	list.common.msgType = CB_FORMAT_LIST;
	list.numFormats = n;
	list.formats = formats;
	UINT rc = c->ServerFormatList(c, &list);
	free(formats);
	return rc;
}

UINT wayrdp_cliprdr_send_format_list_response(CliprdrServerContext* c, BOOL ok)
{
	CLIPRDR_FORMAT_LIST_RESPONSE r = { 0 };
	r.common.msgType = CB_FORMAT_LIST_RESPONSE;
	r.common.msgFlags = ok ? CB_RESPONSE_OK : CB_RESPONSE_FAIL;
	return c->ServerFormatListResponse(c, &r);
}

UINT wayrdp_cliprdr_send_data_request(CliprdrServerContext* c, UINT32 format_id)
{
	CLIPRDR_FORMAT_DATA_REQUEST r = { 0 };
	r.common.msgType = CB_FORMAT_DATA_REQUEST;
	r.requestedFormatId = format_id;
	return c->ServerFormatDataRequest(c, &r);
}

UINT wayrdp_cliprdr_send_data_response(CliprdrServerContext* c, BOOL ok, const BYTE* data,
                                       UINT32 len)
{
	CLIPRDR_FORMAT_DATA_RESPONSE r = { 0 };
	r.common.msgType = CB_FORMAT_DATA_RESPONSE;
	r.common.msgFlags = ok ? CB_RESPONSE_OK : CB_RESPONSE_FAIL;
	r.common.dataLen = ok ? len : 0;
	r.requestedFormatData = ok ? data : NULL;
	return c->ServerFormatDataResponse(c, &r);
}

UINT wayrdp_cliprdr_send_file_contents_request(CliprdrServerContext* c, UINT32 stream_id,
                                               UINT32 list_index, UINT32 flags,
                                               UINT64 position, UINT32 cb_requested)
{
	CLIPRDR_FILE_CONTENTS_REQUEST r = { 0 };
	r.common.msgType = CB_FILECONTENTS_REQUEST;
	r.streamId = stream_id;
	r.listIndex = list_index;
	r.dwFlags = flags;
	r.nPositionLow = (UINT32)(position & 0xFFFFFFFF);
	r.nPositionHigh = (UINT32)(position >> 32);
	r.cbRequested = cb_requested;
	return c->ServerFileContentsRequest(c, &r);
}

UINT wayrdp_cliprdr_send_file_contents_response(CliprdrServerContext* c, UINT32 stream_id,
                                                BOOL ok, const BYTE* data, UINT32 len)
{
	CLIPRDR_FILE_CONTENTS_RESPONSE r = { 0 };
	r.common.msgType = CB_FILECONTENTS_RESPONSE;
	r.common.msgFlags = ok ? CB_RESPONSE_OK : CB_RESPONSE_FAIL;
	r.common.dataLen = ok ? 4 + len : 4;
	r.streamId = stream_id;
	r.cbRequested = ok ? len : 0;
	r.requestedData = ok ? data : NULL;
	return c->ServerFileContentsResponse(c, &r);
}

/* F1: RDP->evdev scancode conversion via WinPR's key tables. The wlroots
 * virtual keyboard must not simply subtract 8 from the RDP scancode — that
 * skips VK mapping, dropping KBDSPECIAL/KBDNUMPAD keys (numpad Enter, Pause)
 * and extended keys the linear map gets wrong (e.g. 0x11D). WinPR's
 * GetVirtualKeyCodeFromVirtualScanCode handles the extended (0x0100),
 * special (0x0400) and numpad (0x0800) classes per MS-RDP-IOT. */
DWORD wayrdp_rdp_scancode_to_evdev(DWORD scancode)
{
	DWORD vk = GetVirtualKeyCodeFromVirtualScanCode(scancode, WINPR_KBD_TYPE_IBM_ENHANCED);
	if (vk == VK_NONE || vk == 0)
		return 0;
	DWORD key = GetKeycodeFromVirtualKeyCode(vk, WINPR_KEYCODE_TYPE_EVDEV);
	return (key == 0 || key == 0xFFFF) ? 0 : key;
}
