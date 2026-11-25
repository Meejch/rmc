/* See shim.h. Written against FreeRDP 3.33.1 (tools/freerdp/build.sh). */
#include "shim.h"
#include "aadtoken.h"

#include <inttypes.h>
#include <poll.h>
#include <stdio.h>
#include <unistd.h>
#include <stdlib.h>
#include <string.h>

#include <winpr/cmdline.h>
#include <winpr/crt.h>
#include <winpr/interlocked.h>
#include <winpr/synch.h>
#include <winpr/wlog.h>

#include <freerdp/freerdp.h>
#include <freerdp/client.h>
#include <freerdp/client/cliprdr.h>
#include <freerdp/client/channels.h>
#include <freerdp/client/cmdline.h>
#include <freerdp/client/rdpgfx.h>
#include <freerdp/client/rdpdr.h>
#include <freerdp/channels/rdpdr.h>
#include <freerdp/channels/rdpgfx.h>
#include <freerdp/codec/clear.h>
#include <freerdp/codec/color.h>
#include <freerdp/codec/planar.h>
#include <freerdp/codecs.h>
#include <freerdp/event.h>
#include <freerdp/input.h>
#include <freerdp/pointer.h>
#include <freerdp/scancode.h>
#include <freerdp/settings.h>
#include <freerdp/utils/helpers.h>

enum cmd_type
{
	CMD_KEY,
	CMD_MOUSE,
	CMD_ACK,
	CMD_CLIP_FORMATS,
	CMD_CLIP_REQUEST,
	CMD_CLIP_RESPONSE,
	CMD_CLIP_FILE_REQUEST,
	CMD_CLIP_FILE_RESPONSE,
	CMD_ADD_DEVICE
};

typedef struct
{
	enum cmd_type type;
	uint32_t a, b, c, d;
	uint8_t* blob; /* owned by the queue entry (clipboard data, format list) */
	size_t blob_len;
} rdpc_cmd;

struct rdpc_session
{
	rdpContext* context;
	rdpc_event_fn fn;
	void* user;
	uint32_t auth;
	RdpgfxClientContext* gfx;

	CRITICAL_SECTION lock;
	HANDLE wake;
	volatile LONG stop; /* set by rdpc_session_stop: leave the loop and disconnect cleanly */
	uint32_t cert_tofu;
	uint32_t cert_prompt;
	uint32_t auto_reconnect;
	/* own login helper (rmc-login): access tokens via aadtoken.c with refresh token
	 * cache; NULL = FreeRDP's helper flow */
	char* own_login;
	/* clipboard (cliprdr): channel, and the local format list ("id\tname\n" lines) that
	 * is (re)announced after MonitorReady */
	CliprdrClientContext* clip;
	uint8_t* clip_formats;
	size_t clip_formats_len;
	rdpc_cmd* queue;
	size_t queue_len, queue_cap;

	rdpCodecs* codecs; /* planar + ClearCodec, decoded on the CPU */
	uint32_t codecs_w, codecs_h;
	gdiPalette palette;
	BYTE* pixbuf;
	size_t pixcap;
};

typedef struct
{
	rdpClientContext common;
	rdpc_session* session;
} rdpcContext;

static rdpc_session* session_of(rdpContext* context)
{
	return context ? ((rdpcContext*)context)->session : NULL;
}

static void emit(rdpc_session* s, rdpc_event* ev)
{
	if (s && s->fn)
		s->fn(s->user, ev);
}

static void emit_simple(rdpc_session* s, uint32_t kind, uint32_t a, const char* message)
{
	rdpc_event ev = { 0 };
	ev.kind = kind;
	ev.a = a;
	ev.message = message;
	emit(s, &ev);
}

/* ---- graphics pipeline callbacks ------------------------------------------------------ */

static rdpc_session* gfx_session(RdpgfxClientContext* gfx)
{
	return gfx ? (rdpc_session*)gfx->custom : NULL;
}

static UINT gfx_on_open(RdpgfxClientContext* gfx, BOOL* do_caps_advertise, BOOL* send_frame_acks)
{
	WINPR_UNUSED(gfx);
	*do_caps_advertise = TRUE;
	/* Acknowledgements are sent by the client after the frame has been presented. */
	*send_frame_acks = FALSE;
	return CHANNEL_RC_OK;
}

static UINT gfx_on_close(RdpgfxClientContext* gfx)
{
	emit_simple(gfx_session(gfx), RDPC_EV_GFX_CLOSED, 0, NULL);
	return CHANNEL_RC_OK;
}

static UINT gfx_caps_confirm(RdpgfxClientContext* gfx, const RDPGFX_CAPS_CONFIRM_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_CAPS_CONFIRM;
	if (pdu && pdu->capsSet)
	{
		ev.a = pdu->capsSet->version;
		ev.b = pdu->capsSet->flags;
	}
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_reset_graphics(RdpgfxClientContext* gfx, const RDPGFX_RESET_GRAPHICS_PDU* pdu)
{
	rdpc_session* s = gfx_session(gfx);
	rdpc_monitor mons[16] = { 0 };
	uint32_t n = pdu->monitorCount > 16 ? 16 : pdu->monitorCount;
	for (uint32_t i = 0; i < n; i++)
	{
		const MONITOR_DEF* m = &pdu->monitorDefArray[i];
		/* TS_MONITOR_DEF uses inclusive right/bottom. */
		mons[i].left = m->left;
		mons[i].top = m->top;
		mons[i].right = m->right + 1;
		mons[i].bottom = m->bottom + 1;
		mons[i].primary = (m->flags & MONITOR_PRIMARY) ? 1 : 0;
	}
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_RESET_GRAPHICS;
	ev.a = pdu->width;
	ev.b = pdu->height;
	ev.monitors = mons;
	ev.monitor_count = n;
	emit(s, &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_start_frame(RdpgfxClientContext* gfx, const RDPGFX_START_FRAME_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_START_FRAME;
	ev.a = pdu->frameId;
	ev.b = pdu->timestamp;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_end_frame(RdpgfxClientContext* gfx, const RDPGFX_END_FRAME_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_END_FRAME;
	ev.a = pdu->frameId;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static rdpc_rect cmd_rect(const RDPGFX_SURFACE_COMMAND* cmd)
{
	rdpc_rect r;
	r.left = (uint16_t)cmd->left;
	r.top = (uint16_t)cmd->top;
	r.right = (uint16_t)cmd->right;
	r.bottom = (uint16_t)cmd->bottom;
	return r;
}

static BYTE* pixbuf(rdpc_session* s, size_t size)
{
	if (size > s->pixcap)
	{
		BYTE* p = realloc(s->pixbuf, size);
		if (!p)
			return NULL;
		s->pixbuf = p;
		s->pixcap = size;
	}
	return s->pixbuf;
}

static BOOL prepare_codecs(rdpc_session* s)
{
	rdpSettings* settings = s->context->settings;
	const UINT32 w = freerdp_settings_get_uint32(settings, FreeRDP_DesktopWidth);
	const UINT32 h = freerdp_settings_get_uint32(settings, FreeRDP_DesktopHeight);
	if (!s->codecs)
	{
		s->codecs = freerdp_client_codecs_new(0);
		if (!s->codecs)
			return FALSE;
	}
	if (s->codecs_w != w || s->codecs_h != h)
	{
		if (!freerdp_client_codecs_prepare(s->codecs, FREERDP_CODEC_PLANAR | FREERDP_CODEC_CLEARCODEC,
		                                   w, h))
			return FALSE;
		s->codecs_w = w;
		s->codecs_h = h;
	}
	return TRUE;
}

/* Non-AVC codecs: decode on the CPU into tight BGRA (PIXEL_FORMAT_BGRA32 = bytes B,G,R,A). */
static UINT surface_pixels(rdpc_session* s, const RDPGFX_SURFACE_COMMAND* cmd)
{
	const size_t stride = 4ull * cmd->width;
	const size_t size = stride * cmd->height;
	if (cmd->width == 0 || cmd->height == 0)
		return CHANNEL_RC_OK;
	BYTE* dst = pixbuf(s, size);
	if (!dst)
		return CHANNEL_RC_NO_MEMORY;
	BOOL ok = FALSE;
	switch (cmd->codecId)
	{
		case RDPGFX_CODECID_UNCOMPRESSED:
		{
			const size_t need = 1ull * FreeRDPGetBytesPerPixel(cmd->format) * cmd->width * cmd->height;
			if (cmd->length < need)
				return ERROR_INVALID_DATA;
			ok = freerdp_image_copy_no_overlap(dst, PIXEL_FORMAT_BGRA32, (UINT32)stride, 0, 0,
			                                   cmd->width, cmd->height, cmd->data, cmd->format, 0, 0,
			                                   0, NULL, FREERDP_FLIP_NONE);
			break;
		}
		case RDPGFX_CODECID_PLANAR:
			if (!prepare_codecs(s))
				return ERROR_INTERNAL_ERROR;
			ok = freerdp_bitmap_decompress_planar(s->codecs->planar, cmd->data, cmd->length,
			                                      cmd->width, cmd->height, dst, PIXEL_FORMAT_BGRA32,
			                                      (UINT32)stride, 0, 0, cmd->width, cmd->height,
			                                      FALSE);
			break;
		case RDPGFX_CODECID_CLEARCODEC:
			if (!prepare_codecs(s))
				return ERROR_INTERNAL_ERROR;
			ok = clear_decompress(s->codecs->clear, cmd->data, cmd->length, cmd->width,
			                      cmd->height, dst, PIXEL_FORMAT_BGRA32, (UINT32)stride, 0, 0,
			                      cmd->width, cmd->height, &s->palette) >= 0;
			break;
		default:
			break;
	}
	if (!ok)
	{
		emit_simple(s, RDPC_EV_ERROR, cmd->codecId, "CPU codec failed to decode");
		return ERROR_INTERNAL_ERROR;
	}
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_PIXELS;
	ev.surface = cmd->surfaceId;
	ev.rect = cmd_rect(cmd);
	ev.data = dst;
	ev.length = (uint32_t)size;
	emit(s, &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_surface_command(RdpgfxClientContext* gfx, const RDPGFX_SURFACE_COMMAND* cmd)
{
	rdpc_session* s = gfx_session(gfx);
	rdpc_event ev = { 0 };
	ev.surface = cmd->surfaceId;
	ev.rect = cmd_rect(cmd);
	switch (cmd->codecId)
	{
		case RDPGFX_CODECID_AVC420:
		{
			const RDPGFX_AVC420_BITMAP_STREAM* bs = cmd->extra;
			if (!bs)
				return ERROR_INVALID_DATA;
			ev.kind = RDPC_EV_AVC420;
			ev.rects = (const rdpc_rect*)bs->meta.regionRects;
			ev.rect_count = bs->meta.numRegionRects;
			ev.data = bs->data;
			ev.length = bs->length;
			emit(s, &ev);
			return CHANNEL_RC_OK;
		}
		case RDPGFX_CODECID_AVC444:
		case RDPGFX_CODECID_AVC444v2:
		{
			const RDPGFX_AVC444_BITMAP_STREAM* bs = cmd->extra;
			if (!bs)
				return ERROR_INVALID_DATA;
			ev.kind = RDPC_EV_AVC444;
			ev.a = (cmd->codecId == RDPGFX_CODECID_AVC444v2) ? 2 : 1;
			ev.b = bs->LC;
			/* LC 0: main + aux; 1: main only; 2: aux (chroma) only, carried in stream 1. */
			const RDPGFX_AVC420_BITMAP_STREAM* main = (bs->LC == 2) ? NULL : &bs->bitstream[0];
			const RDPGFX_AVC420_BITMAP_STREAM* aux =
			    (bs->LC == 0) ? &bs->bitstream[1] : ((bs->LC == 2) ? &bs->bitstream[0] : NULL);
			if (main)
			{
				ev.rects = (const rdpc_rect*)main->meta.regionRects;
				ev.rect_count = main->meta.numRegionRects;
				ev.data = main->data;
				ev.length = main->length;
			}
			if (aux)
			{
				ev.rects2 = (const rdpc_rect*)aux->meta.regionRects;
				ev.rect_count2 = aux->meta.numRegionRects;
				ev.data2 = aux->data;
				ev.length2 = aux->length;
			}
			emit(s, &ev);
			return CHANNEL_RC_OK;
		}
		case RDPGFX_CODECID_UNCOMPRESSED:
		case RDPGFX_CODECID_PLANAR:
		case RDPGFX_CODECID_CLEARCODEC:
			return surface_pixels(s, cmd);
		default:
			ev.kind = RDPC_EV_UNSUPPORTED_CODEC;
			ev.a = cmd->codecId;
			emit(s, &ev);
			return CHANNEL_RC_OK;
	}
}

static UINT gfx_delete_encoding_context(RdpgfxClientContext* gfx,
                                        const RDPGFX_DELETE_ENCODING_CONTEXT_PDU* pdu)
{
	WINPR_UNUSED(gfx);
	WINPR_UNUSED(pdu);
	return CHANNEL_RC_OK;
}

static UINT gfx_create_surface(RdpgfxClientContext* gfx, const RDPGFX_CREATE_SURFACE_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_CREATE_SURFACE;
	ev.surface = pdu->surfaceId;
	ev.a = pdu->width;
	ev.b = pdu->height;
	ev.c = pdu->pixelFormat;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_delete_surface(RdpgfxClientContext* gfx, const RDPGFX_DELETE_SURFACE_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_DELETE_SURFACE;
	ev.surface = pdu->surfaceId;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_solid_fill(RdpgfxClientContext* gfx, const RDPGFX_SOLID_FILL_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_SOLID_FILL;
	ev.surface = pdu->surfaceId;
	ev.a = (uint32_t)pdu->fillPixel.B | ((uint32_t)pdu->fillPixel.G << 8) |
	       ((uint32_t)pdu->fillPixel.R << 16) | ((uint32_t)pdu->fillPixel.XA << 24);
	ev.rects = (const rdpc_rect*)pdu->fillRects;
	ev.rect_count = pdu->fillRectCount;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_surface_to_surface(RdpgfxClientContext* gfx,
                                   const RDPGFX_SURFACE_TO_SURFACE_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_SURFACE_TO_SURFACE;
	ev.surface = pdu->surfaceIdSrc;
	ev.surface2 = pdu->surfaceIdDest;
	memcpy(&ev.rect, &pdu->rectSrc, sizeof(ev.rect));
	ev.points = (const rdpc_point*)pdu->destPts;
	ev.point_count = pdu->destPtsCount;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_surface_to_cache(RdpgfxClientContext* gfx, const RDPGFX_SURFACE_TO_CACHE_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_SURFACE_TO_CACHE;
	ev.surface = pdu->surfaceId;
	memcpy(&ev.rect, &pdu->rectSrc, sizeof(ev.rect));
	ev.a = pdu->cacheSlot;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_cache_to_surface(RdpgfxClientContext* gfx, const RDPGFX_CACHE_TO_SURFACE_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_CACHE_TO_SURFACE;
	ev.a = pdu->cacheSlot;
	ev.surface = pdu->surfaceId;
	ev.points = (const rdpc_point*)pdu->destPts;
	ev.point_count = pdu->destPtsCount;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_cache_import_reply(RdpgfxClientContext* gfx,
                                   const RDPGFX_CACHE_IMPORT_REPLY_PDU* pdu)
{
	/* No persistent cache is offered, so there is nothing to import. */
	WINPR_UNUSED(gfx);
	WINPR_UNUSED(pdu);
	return CHANNEL_RC_OK;
}

static UINT gfx_evict_cache_entry(RdpgfxClientContext* gfx,
                                  const RDPGFX_EVICT_CACHE_ENTRY_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_EVICT_CACHE;
	ev.a = pdu->cacheSlot;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_map_surface_to_output(RdpgfxClientContext* gfx,
                                      const RDPGFX_MAP_SURFACE_TO_OUTPUT_PDU* pdu)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_MAP_SURFACE;
	ev.surface = pdu->surfaceId;
	ev.a = pdu->outputOriginX;
	ev.b = pdu->outputOriginY;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_map_surface_to_scaled_output(RdpgfxClientContext* gfx,
                                             const RDPGFX_MAP_SURFACE_TO_SCALED_OUTPUT_PDU* pdu)
{
	/* Treated like an unscaled mapping for now (target size is logged by the caller). */
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_MAP_SURFACE;
	ev.surface = pdu->surfaceId;
	ev.a = pdu->outputOriginX;
	ev.b = pdu->outputOriginY;
	ev.c = pdu->targetWidth;
	ev.d = pdu->targetHeight;
	emit(gfx_session(gfx), &ev);
	return CHANNEL_RC_OK;
}

static UINT gfx_update_surfaces(RdpgfxClientContext* gfx)
{
	WINPR_UNUSED(gfx);
	return CHANNEL_RC_OK;
}

static void gfx_init(rdpc_session* s, RdpgfxClientContext* gfx)
{
	s->gfx = gfx;
	gfx->custom = s;
	gfx->OnOpen = gfx_on_open;
	gfx->OnClose = gfx_on_close;
	gfx->CapsConfirm = gfx_caps_confirm;
	gfx->ResetGraphics = gfx_reset_graphics;
	gfx->StartFrame = gfx_start_frame;
	gfx->EndFrame = gfx_end_frame;
	gfx->SurfaceCommand = gfx_surface_command;
	gfx->DeleteEncodingContext = gfx_delete_encoding_context;
	gfx->CreateSurface = gfx_create_surface;
	gfx->DeleteSurface = gfx_delete_surface;
	gfx->SolidFill = gfx_solid_fill;
	gfx->SurfaceToSurface = gfx_surface_to_surface;
	gfx->SurfaceToCache = gfx_surface_to_cache;
	gfx->CacheToSurface = gfx_cache_to_surface;
	gfx->CacheImportReply = gfx_cache_import_reply;
	gfx->EvictCacheEntry = gfx_evict_cache_entry;
	gfx->MapSurfaceToOutput = gfx_map_surface_to_output;
	gfx->MapSurfaceToScaledOutput = gfx_map_surface_to_scaled_output;
	gfx->UpdateSurfaces = gfx_update_surfaces;
}

/* ---- clipboard (cliprdr, MS-RDPECLIP) ------------------------------------------------- */

/* Text-only format list exchange with the client: "id\tname\n" per format, name empty for
 * predefined formats (CF_UNICODETEXT = 13, CF_DIB = 8, ...). Data moves on request in both
 * directions; conversions (UTF-16, PNG/DIB) happen in the Rust client. */

static rdpc_session* clip_session(CliprdrClientContext* clip)
{
	return clip ? (rdpc_session*)clip->custom : NULL;
}

/* Announces the client's (local) formats from the "id\tname\n" list. */
static UINT clip_send_format_list(rdpc_session* s, const uint8_t* list, size_t len)
{
	if (!s || !s->clip || !s->clip->ClientFormatList)
		return CHANNEL_RC_OK;
	CLIPRDR_FORMAT formats[32] = { 0 };
	char* copy = calloc(len + 1, 1);
	if (!copy)
		return CHANNEL_RC_NO_MEMORY;
	if (len)
		memcpy(copy, list, len);
	UINT32 n = 0;
	char* save = NULL;
	for (char* line = strtok_r(copy, "\n", &save); line && n < ARRAYSIZE(formats);
	     line = strtok_r(NULL, "\n", &save))
	{
		char* tab = strchr(line, '\t');
		if (tab)
			*tab = '\0';
		formats[n].formatId = (UINT32)strtoul(line, NULL, 10);
		formats[n].formatName = (tab && tab[1]) ? tab + 1 : NULL;
		n++;
	}
	CLIPRDR_FORMAT_LIST fl = { 0 };
	fl.common.msgType = CB_FORMAT_LIST;
	fl.numFormats = n;
	fl.formats = formats;
	const UINT rc = s->clip->ClientFormatList(s->clip, &fl);
	free(copy);
	return rc;
}

static UINT clip_monitor_ready(CliprdrClientContext* clip, const CLIPRDR_MONITOR_READY* ready)
{
	WINPR_UNUSED(ready);
	rdpc_session* s = clip_session(clip);
	CLIPRDR_GENERAL_CAPABILITY_SET general = { 0 };
	general.capabilitySetType = CB_CAPSTYPE_GENERAL;
	general.capabilitySetLength = 12;
	general.version = CB_CAPS_VERSION_2;
	general.generalFlags = CB_USE_LONG_FORMAT_NAMES | CB_STREAM_FILECLIP_ENABLED |
	                       CB_FILECLIP_NO_FILE_PATHS | CB_HUGE_FILE_SUPPORT_ENABLED;
	CLIPRDR_CAPABILITIES caps = { 0 };
	caps.cCapabilitiesSets = 1;
	caps.capabilitySets = (CLIPRDR_CAPABILITY_SET*)&general;
	UINT rc = clip->ClientCapabilities(clip, &caps);
	if (rc != CHANNEL_RC_OK)
		return rc;
	rc = clip_send_format_list(s, s ? s->clip_formats : NULL, s ? s->clip_formats_len : 0);
	emit_simple(s, RDPC_EV_CLIP_READY, 0, NULL);
	return rc;
}

static UINT clip_server_caps(CliprdrClientContext* clip, const CLIPRDR_CAPABILITIES* caps)
{
	WINPR_UNUSED(clip);
	WINPR_UNUSED(caps);
	return CHANNEL_RC_OK;
}

static UINT clip_server_format_list(CliprdrClientContext* clip, const CLIPRDR_FORMAT_LIST* fl)
{
	rdpc_session* s = clip_session(clip);
	CLIPRDR_FORMAT_LIST_RESPONSE resp = { 0 };
	resp.common.msgType = CB_FORMAT_LIST_RESPONSE;
	resp.common.msgFlags = CB_RESPONSE_OK;
	const UINT rc = clip->ClientFormatListResponse(clip, &resp);
	/* "id\tname\n" lines for the client */
	size_t cap = 64;
	for (UINT32 i = 0; i < fl->numFormats; i++)
		cap += 16 + (fl->formats[i].formatName ? strlen(fl->formats[i].formatName) : 0);
	char* buf = calloc(cap, 1);
	if (!buf)
		return CHANNEL_RC_NO_MEMORY;
	size_t len = 0;
	for (UINT32 i = 0; i < fl->numFormats; i++)
		len += (size_t)snprintf(buf + len, cap - len, "%" PRIu32 "\t%s\n", fl->formats[i].formatId,
		                        fl->formats[i].formatName ? fl->formats[i].formatName : "");
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_CLIP_REMOTE_FORMATS;
	ev.data = (const uint8_t*)buf;
	ev.length = (uint32_t)len;
	emit(s, &ev);
	free(buf);
	return rc;
}

static UINT clip_server_format_list_response(CliprdrClientContext* clip,
                                             const CLIPRDR_FORMAT_LIST_RESPONSE* resp)
{
	WINPR_UNUSED(clip);
	WINPR_UNUSED(resp);
	return CHANNEL_RC_OK;
}

static UINT clip_server_data_request(CliprdrClientContext* clip,
                                     const CLIPRDR_FORMAT_DATA_REQUEST* req)
{
	emit_simple(clip_session(clip), RDPC_EV_CLIP_DATA_REQUEST, req->requestedFormatId, NULL);
	return CHANNEL_RC_OK;
}

static UINT clip_server_data_response(CliprdrClientContext* clip,
                                      const CLIPRDR_FORMAT_DATA_RESPONSE* resp)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_CLIP_DATA;
	ev.a = (resp->common.msgFlags & CB_RESPONSE_OK) ? 1 : 0;
	ev.data = resp->requestedFormatData;
	ev.length = resp->common.dataLen;
	emit(clip_session(clip), &ev);
	return CHANNEL_RC_OK;
}

/* File contents (copying files): the server asks for a file's size or a range. */
static UINT clip_server_file_request(CliprdrClientContext* clip,
                                     const CLIPRDR_FILE_CONTENTS_REQUEST* req)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_CLIP_FILE_REQUEST;
	ev.a = req->streamId;
	ev.b = req->listIndex;
	ev.c = req->dwFlags;
	ev.d = req->cbRequested;
	ev.surface = req->nPositionLow;
	ev.surface2 = req->nPositionHigh;
	emit(clip_session(clip), &ev);
	return CHANNEL_RC_OK;
}

static UINT clip_server_file_response(CliprdrClientContext* clip,
                                      const CLIPRDR_FILE_CONTENTS_RESPONSE* resp)
{
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_CLIP_FILE_DATA;
	ev.a = resp->streamId;
	ev.b = (resp->common.msgFlags & CB_RESPONSE_OK) ? 1 : 0;
	ev.data = resp->requestedData;
	ev.length = resp->cbRequested;
	emit(clip_session(clip), &ev);
	return CHANNEL_RC_OK;
}

static void clip_init(rdpc_session* s, CliprdrClientContext* clip)
{
	s->clip = clip;
	clip->custom = s;
	clip->MonitorReady = clip_monitor_ready;
	clip->ServerCapabilities = clip_server_caps;
	clip->ServerFormatList = clip_server_format_list;
	clip->ServerFormatListResponse = clip_server_format_list_response;
	clip->ServerFormatDataRequest = clip_server_data_request;
	clip->ServerFormatDataResponse = clip_server_data_response;
	clip->ServerFileContentsRequest = clip_server_file_request;
	clip->ServerFileContentsResponse = clip_server_file_response;
}

/* ---- channels and connection callbacks ------------------------------------------------ */

static void on_channel_connected(void* context, const ChannelConnectedEventArgs* e)
{
	rdpcContext* c = (rdpcContext*)context;
	if (strcmp(e->name, RDPGFX_DVC_CHANNEL_NAME) == 0)
		gfx_init(c->session, (RdpgfxClientContext*)e->pInterface);
	else if (strcmp(e->name, CLIPRDR_SVC_CHANNEL_NAME) == 0)
		clip_init(c->session, (CliprdrClientContext*)e->pInterface);
	else
		freerdp_client_OnChannelConnectedEventHandler(&c->common, e);
}

static void on_channel_disconnected(void* context, const ChannelDisconnectedEventArgs* e)
{
	rdpcContext* c = (rdpcContext*)context;
	if (strcmp(e->name, RDPGFX_DVC_CHANNEL_NAME) == 0)
	{
		RdpgfxClientContext* gfx = (RdpgfxClientContext*)e->pInterface;
		gfx->custom = NULL;
		c->session->gfx = NULL;
	}
	else if (strcmp(e->name, CLIPRDR_SVC_CHANNEL_NAME) == 0)
	{
		CliprdrClientContext* clip = (CliprdrClientContext*)e->pInterface;
		clip->custom = NULL;
		c->session->clip = NULL;
	}
	else
		freerdp_client_OnChannelDisconnectedEventHandler(&c->common, e);
}

static BOOL noop_paint(rdpContext* context)
{
	WINPR_UNUSED(context);
	return TRUE;
}

static BOOL noop_bitmap(rdpContext* context, const BITMAP_UPDATE* bitmap)
{
	WINPR_UNUSED(context);
	WINPR_UNUSED(bitmap);
	return TRUE;
}

static BOOL pointer_position(rdpContext* c, const POINTER_POSITION_UPDATE* p)
{
	/* Server-side pointer warps cannot be honoured on Wayland. */
	WINPR_UNUSED(c);
	WINPR_UNUSED(p);
	return TRUE;
}

static BOOL pointer_system(rdpContext* c, const POINTER_SYSTEM_UPDATE* p)
{
	emit_simple(session_of(c), RDPC_EV_POINTER_SYSTEM, p->type == SYSPTR_NULL ? 0 : 1, NULL);
	return TRUE;
}

/* Converts XOR/AND masks (1..32 bpp) to tight BGRA and emits the shape. */
static BOOL pointer_set(rdpContext* c, UINT32 xorBpp, const POINTER_COLOR_UPDATE* p, UINT32 lenAnd,
                        UINT32 lenXor)
{
	rdpc_session* s = session_of(c);
	if (!s)
		return FALSE;
	if (p->width == 0 || p->height == 0)
	{
		emit_simple(s, RDPC_EV_POINTER_SYSTEM, 0, NULL);
		return TRUE;
	}
	const size_t stride = 4ull * p->width;
	const size_t size = stride * p->height;
	BYTE* dst = pixbuf(s, size);
	if (!dst)
		return FALSE;
	if (!freerdp_image_copy_from_pointer_data(dst, PIXEL_FORMAT_BGRA32, (UINT32)stride, 0, 0,
	                                          p->width, p->height, p->xorMaskData, lenXor,
	                                          p->andMaskData, lenAnd, xorBpp, &s->palette))
	{
		emit_simple(s, RDPC_EV_ERROR, xorBpp, "pointer conversion failed");
		return TRUE;
	}
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_POINTER_SET;
	ev.a = p->cacheIndex;
	ev.b = p->width;
	ev.c = p->height;
	ev.d = (uint32_t)p->hotSpotX | ((uint32_t)p->hotSpotY << 16);
	ev.data = dst;
	ev.length = (uint32_t)size;
	emit(s, &ev);
	return TRUE;
}

static BOOL pointer_color(rdpContext* c, const POINTER_COLOR_UPDATE* p)
{
	return pointer_set(c, 24, p, p->lengthAndMask, p->lengthXorMask);
}

static BOOL pointer_new(rdpContext* c, const POINTER_NEW_UPDATE* p)
{
	return pointer_set(c, p->xorBpp, &p->colorPtrAttr, p->colorPtrAttr.lengthAndMask,
	                   p->colorPtrAttr.lengthXorMask);
}

static BOOL pointer_large(rdpContext* c, const POINTER_LARGE_UPDATE* p)
{
	POINTER_COLOR_UPDATE tmp = { 0 };
	tmp.cacheIndex = p->cacheIndex;
	tmp.hotSpotX = p->hotSpotX;
	tmp.hotSpotY = p->hotSpotY;
	tmp.width = p->width;
	tmp.height = p->height;
	tmp.xorMaskData = p->xorMaskData;
	tmp.andMaskData = p->andMaskData;
	return pointer_set(c, p->xorBpp, &tmp, p->lengthAndMask, p->lengthXorMask);
}

static BOOL pointer_cached(rdpContext* c, const POINTER_CACHED_UPDATE* p)
{
	emit_simple(session_of(c), RDPC_EV_POINTER_CACHED, p->cacheIndex, NULL);
	return TRUE;
}

static BOOL pre_connect(freerdp* instance)
{
	rdpContext* context = instance->context;
	if (PubSub_SubscribeChannelConnected(context->pubSub, on_channel_connected) < 0)
		return FALSE;
	if (PubSub_SubscribeChannelDisconnected(context->pubSub, on_channel_disconnected) < 0)
		return FALSE;
	return TRUE;
}

static BOOL post_connect(freerdp* instance)
{
	rdpContext* context = instance->context;
	rdpUpdate* update = context->update;
	/* All graphics go through the graphics pipeline into the GPU core; no software GDI. */
	if (!freerdp_settings_set_bool(context->settings, FreeRDP_DeactivateClientDecoding, TRUE))
		return FALSE;
	update->BeginPaint = noop_paint;
	update->EndPaint = noop_paint;
	update->DesktopResize = noop_paint;
	update->BitmapUpdate = noop_bitmap;
	update->pointer->PointerPosition = pointer_position;
	update->pointer->PointerSystem = pointer_system;
	update->pointer->PointerColor = pointer_color;
	update->pointer->PointerNew = pointer_new;
	update->pointer->PointerCached = pointer_cached;
	update->pointer->PointerLarge = pointer_large;
	rdpc_event ev = { 0 };
	ev.kind = RDPC_EV_CONNECTED;
	ev.a = freerdp_settings_get_uint32(context->settings, FreeRDP_DesktopWidth);
	ev.b = freerdp_settings_get_uint32(context->settings, FreeRDP_DesktopHeight);
	emit(session_of(context), &ev);
	return TRUE;
}

static void post_disconnect(freerdp* instance)
{
	rdpContext* context = instance->context;
	PubSub_UnsubscribeChannelConnected(context->pubSub, on_channel_connected);
	PubSub_UnsubscribeChannelDisconnected(context->pubSub, on_channel_disconnected);
}

/* Copies a field for the prompt line: tabs and line breaks become spaces. */
static void field(char* dst, size_t len, const char* src)
{
	size_t i = 0;
	for (; src && src[i] && i + 1 < len; i++)
		dst[i] = (src[i] == '\t' || src[i] == '\n' || src[i] == '\r') ? ' ' : src[i];
	dst[i] = '\0';
}

/* Asks the launcher (or a terminal user) whether to trust a certificate: one line on stdout
 * "RDPC-CERT\t<new|changed>\t<host:port>\t<common name>\t<subject>\t<issuer>\t<fingerprint>
 * \t<old fingerprint>", answer "yes" or "no" on stdin within 5 minutes. */
static BOOL ask_certificate(const char* kind, const char* host, UINT16 port, const char* cn,
                            const char* subject, const char* issuer, const char* fp,
                            const char* old_fp)
{
	char f[6][512];
	field(f[0], sizeof(f[0]), host);
	field(f[1], sizeof(f[1]), cn);
	field(f[2], sizeof(f[2]), subject);
	field(f[3], sizeof(f[3]), issuer);
	field(f[4], sizeof(f[4]), fp);
	field(f[5], sizeof(f[5]), old_fp);
	fflush(stderr);
	printf("RDPC-CERT\t%s\t%s:%u\t%s\t%s\t%s\t%s\t%s\n", kind, f[0], port, f[1], f[2], f[3],
	       f[4], f[5]);
	fflush(stdout);
	struct pollfd p = { .fd = STDIN_FILENO, .events = POLLIN };
	if (poll(&p, 1, 300000) <= 0)
		return FALSE;
	char line[16] = { 0 };
	const ssize_t n = read(STDIN_FILENO, line, sizeof(line) - 1);
	return n >= 3 && strncmp(line, "yes", 3) == 0;
}

static DWORD verify_certificate(freerdp* instance, const char* host, UINT16 port,
                                const char* common_name, const char* subject, const char* issuer,
                                const char* fingerprint, DWORD flags)
{
	WINPR_UNUSED(flags);
	rdpc_session* s = session_of(instance->context);
	char msg[512];
	if (s && s->cert_prompt)
	{
		const BOOL ok =
		    ask_certificate("new", host, port, common_name, subject, issuer, fingerprint, NULL);
		snprintf(msg, sizeof(msg), "certificate of %s:%u (%s) %s, fingerprint %s", host, port,
		         common_name ? common_name : "?",
		         ok ? "trusted by the user and stored" : "rejected by the user",
		         fingerprint ? fingerprint : "?");
		emit_simple(s, ok ? RDPC_EV_INFO : RDPC_EV_ERROR, 0, msg);
		return ok ? 1 : 0;
	}
	if (s && s->cert_tofu)
	{
		snprintf(msg, sizeof(msg),
		         "certificate of %s:%u (%s) accepted on first use and stored, fingerprint %s", host,
		         port, common_name ? common_name : "?", fingerprint ? fingerprint : "?");
		emit_simple(s, RDPC_EV_INFO, 0, msg);
		return 1; /* accept and store */
	}
	snprintf(msg, sizeof(msg), "unknown certificate of %s:%u rejected, fingerprint %s", host, port,
	         fingerprint ? fingerprint : "?");
	emit_simple(s, RDPC_EV_ERROR, 0, msg);
	return 0;
}

/* Entra-joined hosts present a short-lived device certificate (issuer "MS-Organization-P2P-
 * Access [<year>]", about 24 h) that Windows renews at least daily. A change is accepted when
 * issuer and subject (device and tenant id) are unchanged; any other change is rejected.
 * Interim rule (decided 24.11.2026); verifying the signature against the tenant's P2P root
 * certificate replaces it later. */
static BOOL entra_renewal(const char* subject, const char* issuer, const char* old_subject,
                          const char* old_issuer)
{
	static const char p2p[] = "CN=MS-Organization-P2P-Access [";
	if (!subject || !issuer || !old_subject || !old_issuer || !subject[0])
		return FALSE;
	if (strcmp(subject, old_subject) != 0 || strcmp(issuer, old_issuer) != 0)
		return FALSE;
	/* Tolerate "CN = x" as well as "CN=x" spellings of the issuer. */
	char norm[256];
	size_t n = 0;
	for (const char* c = issuer; *c && n + 1 < sizeof(norm); c++)
		if (*c != ' ' || (n > 0 && norm[n - 1] != '=' && c[1] != '='))
			norm[n++] = *c;
	norm[n] = '\0';
	return strncmp(norm, p2p, sizeof(p2p) - 1) == 0;
}

static DWORD verify_changed_certificate(freerdp* instance, const char* host, UINT16 port,
                                        const char* common_name, const char* subject,
                                        const char* issuer, const char* new_fingerprint,
                                        const char* old_subject, const char* old_issuer,
                                        const char* old_fingerprint, DWORD flags)
{
	WINPR_UNUSED(common_name);
	WINPR_UNUSED(flags);
	char msg[768];
	if (entra_renewal(subject, issuer, old_subject, old_issuer))
	{
		snprintf(msg, sizeof(msg),
		         "certificate of %s:%u renewed by Entra (same device, tenant and issuer %s), "
		         "accepted and stored: %s",
		         host, port, issuer, new_fingerprint ? new_fingerprint : "?");
		emit_simple(session_of(instance->context), RDPC_EV_INFO, 0, msg);
		return 1; /* accept and replace the stored certificate */
	}
	rdpc_session* s = session_of(instance->context);
	if (s && s->cert_prompt)
	{
		const BOOL ok = ask_certificate("changed", host, port, common_name, subject, issuer,
		                                new_fingerprint, old_fingerprint);
		snprintf(msg, sizeof(msg), "changed certificate of %s:%u %s: %s (before %s)", host, port,
		         ok ? "trusted by the user and stored" : "rejected by the user",
		         new_fingerprint ? new_fingerprint : "?", old_fingerprint ? old_fingerprint : "?");
		emit_simple(s, ok ? RDPC_EV_INFO : RDPC_EV_ERROR, 0, msg);
		return ok ? 1 : 0;
	}
	snprintf(msg, sizeof(msg),
	         "certificate of %s:%u CHANGED, connection rejected (stored %s, now %s; subject "
	         "\"%s\" -> \"%s\", issuer \"%s\" -> \"%s\"); remove the stored certificate under "
	         "~/.config/rmc/freerdp/server/ only if the change is expected",
	         host, port, old_fingerprint ? old_fingerprint : "?",
	         new_fingerprint ? new_fingerprint : "?", old_subject ? old_subject : "?",
	         subject ? subject : "?", old_issuer ? old_issuer : "?", issuer ? issuer : "?");
	emit_simple(session_of(instance->context), RDPC_EV_ERROR, 0, msg);
	return 0;
}

/* AAD with the own login helper: refresh token cache (aadtoken.c); everything else as
 * FreeRDP's helper flow. */
static BOOL get_access_token(freerdp* instance, AccessTokenType type, char** token, size_t count,
                             ...)
{
	rdpc_session* s = session_of(instance->context);
	if (s && s->own_login && type == ACCESS_TOKEN_TYPE_AAD && count >= 2)
	{
		va_list ap;
		va_start(ap, count);
		const char* scope = va_arg(ap, const char*);
		const char* req_cnf = va_arg(ap, const char*);
		va_end(ap);
		return rdpc_aad_access_token(instance, s->own_login, scope, req_cnf, token);
	}
	if (type == ACCESS_TOKEN_TYPE_AAD && count >= 2)
	{
		va_list ap;
		va_start(ap, count);
		const char* scope = va_arg(ap, const char*);
		const char* req_cnf = va_arg(ap, const char*);
		va_end(ap);
		return client_helper_get_access_token(instance, type, token, 2, scope, req_cnf);
	}
	return client_helper_get_access_token(instance, type, token, 0);
}

static BOOL client_new(freerdp* instance, rdpContext* context)
{
	WINPR_UNUSED(context);
	instance->PreConnect = pre_connect;
	instance->PostConnect = post_connect;
	instance->PostDisconnect = post_disconnect;
	instance->VerifyCertificateEx = verify_certificate;
	instance->VerifyChangedCertificateEx = verify_changed_certificate;
	/* AAD: token from the out-of-process login helper (FreeRDP_AadAuthHelper); no console
	 * fallback, the client is not interactive on stdin. */
	instance->GetAccessToken = get_access_token;
	return TRUE;
}

static void client_free(freerdp* instance, rdpContext* context)
{
	WINPR_UNUSED(instance);
	WINPR_UNUSED(context);
}

static int client_entry(RDP_CLIENT_ENTRY_POINTS* ep)
{
	ZeroMemory(ep, sizeof(RDP_CLIENT_ENTRY_POINTS));
	ep->Version = RDP_CLIENT_INTERFACE_VERSION;
	ep->Size = sizeof(RDP_CLIENT_ENTRY_POINTS_V1);
	ep->ContextSize = sizeof(rdpcContext);
	ep->ClientNew = client_new;
	ep->ClientFree = client_free;
	return 0;
}

/* ---- settings ------------------------------------------------------------------------- */

static BOOL apply_config(rdpSettings* st, const rdpc_config* cfg)
{
	BOOL ok = TRUE;
	ok &= freerdp_settings_set_string(st, FreeRDP_ServerHostname, cfg->host);
	ok &= freerdp_settings_set_uint32(st, FreeRDP_ServerPort, cfg->port ? cfg->port : 3389);
	if (cfg->username)
		ok &= freerdp_settings_set_string(st, FreeRDP_Username, cfg->username);
	if (cfg->domain)
		ok &= freerdp_settings_set_string(st, FreeRDP_Domain, cfg->domain);
	if (cfg->password)
		ok &= freerdp_settings_set_string(st, FreeRDP_Password, cfg->password);
	ok &= freerdp_settings_set_uint32(st, FreeRDP_DesktopWidth, cfg->width);
	ok &= freerdp_settings_set_uint32(st, FreeRDP_DesktopHeight, cfg->height);
	ok &= freerdp_settings_set_uint32(st, FreeRDP_ColorDepth, 32);
	ok &= freerdp_set_connection_type(st, CONNECTION_TYPE_LAN);

	/* Graphics pipeline with H.264 only: AVC420, optionally AVC444 (v1/v2). */
	ok &= freerdp_settings_set_bool(st, FreeRDP_SupportGraphicsPipeline, TRUE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_GfxH264, TRUE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_GfxAVC444, cfg->avc444 ? TRUE : FALSE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_GfxAVC444v2, cfg->avc444 ? TRUE : FALSE);
	/* Performance flags (MS-RDPBCGR TS_EXTENDED_INFO_PACKET) like mstsc on a LAN: font
	 * smoothing and desktop composition on, wallpaper, themes and full window drag kept.
	 * (Before 24.11.2026 the flags were 0, i.e. no ClearType; sdl-freerdp's set with full window
	 * drag, themes and wallpaper off made the session feel sluggish.) */
	ok &= freerdp_settings_set_bool(st, FreeRDP_AllowDesktopComposition, TRUE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_AllowFontSmoothing, TRUE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_DisableWallpaper, FALSE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_DisableFullWindowDrag, FALSE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_DisableThemes, FALSE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_DisableMenuAnims, FALSE);
	freerdp_performance_flags_make(st);
	/* Small GFX cache (MS-RDPEGFX RDPGFX_CAPS_FLAG_SMALL_CACHE, 4096 slots): matches the
	 * cache size of the GPU core (DEFAULT_CACHE_SLOTS); before, the server was offered the
	 * large cache (25600 slots). */
	ok &= freerdp_settings_set_bool(st, FreeRDP_GfxSmallCache, TRUE);

	ok &= freerdp_settings_set_bool(st, FreeRDP_GfxProgressive, FALSE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_GfxProgressiveV2, FALSE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_RemoteFxCodec, FALSE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_GfxSmallCache, FALSE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_GfxThinClient, FALSE);

	if (cfg->ignore_certificate)
		ok &= freerdp_settings_set_bool(st, FreeRDP_IgnoreCertificate, TRUE);

	if (cfg->auth == RDPC_AUTH_AAD)
	{
		/* Like /sec:aad: RDS AAD as the only security protocol, empty domain. */
		const FreeRDP_Settings_Keys_Bool off[] = { FreeRDP_UseRdpSecurityLayer, FreeRDP_RdpSecurity,
			                                       FreeRDP_NlaSecurity, FreeRDP_ExtSecurity,
			                                       FreeRDP_TlsSecurity };
		for (size_t i = 0; i < ARRAYSIZE(off); i++)
			ok &= freerdp_settings_set_bool(st, off[i], FALSE);
		ok &= freerdp_settings_set_bool(st, FreeRDP_AadSecurity, TRUE);
		ok &= freerdp_settings_set_string(st, FreeRDP_Domain, "");
		if (cfg->aad_helper)
			ok &= freerdp_settings_set_string(st, FreeRDP_AadAuthHelper, cfg->aad_helper);
	}

	/* Audio playback (static + dynamic rdpsnd, like /sound), microphone (audin, like
	 * /microphone), camera (rdpecam, like /dvc:rdpecam). */
	const struct
	{
		const char* name;
		const char* value;
		BOOL static_too;
	} channels[] = { { "rdpsnd", cfg->sound, TRUE },
		             { "audin", cfg->microphone, FALSE },
		             { "rdpecam", cfg->camera, FALSE } };
	for (size_t i = 0; ok && i < ARRAYSIZE(channels); i++)
	{
		if (!channels[i].value)
			continue;
		size_t count = 0;
		char** argv = channels[i].value[0]
		                  ? CommandLineParseCommaSeparatedValuesEx(channels[i].name,
		                                                           channels[i].value, &count)
		                  : CommandLineParseCommaSeparatedValuesEx(channels[i].name, NULL, &count);
		if (!argv)
			return FALSE;
		if (channels[i].static_too)
			ok &= freerdp_client_add_static_channel(st, count, (const char* const*)argv);
		ok &= freerdp_client_add_dynamic_channel(st, count, (const char* const*)argv);
		CommandLineParserFree(argv);
	}
	if (cfg->sound)
	{
		ok &= freerdp_settings_set_bool(st, FreeRDP_AudioPlayback, TRUE);
		ok &= freerdp_settings_set_bool(st, FreeRDP_DeviceRedirection, TRUE);
	}
	if (cfg->microphone)
		ok &= freerdp_settings_set_bool(st, FreeRDP_AudioCapture, TRUE);
	if (cfg->clipboard)
		ok &= freerdp_settings_set_bool(st, FreeRDP_RedirectClipboard, TRUE);
	/* rdpdr is always loaded: folders and printers can also be added during the session
	 * (rdpc_add_device). FreeRDP_RedirectDrives stays off: it would add a "media *" drive
	 * that hotplugs every mounted share (live test 24.11.2026). */
	ok &= freerdp_settings_set_bool(st, FreeRDP_DeviceRedirection, TRUE);
	ok &= freerdp_settings_set_bool(st, FreeRDP_RedirectDrives, FALSE);
	/* redirected CUPS printers, one per line (only these, not all) */
	if (cfg->printers && cfg->printers[0])
	{
		char* copy = _strdup(cfg->printers);
		char* save = NULL;
		for (char* line = copy ? strtok_r(copy, "\n", &save) : NULL; ok && line;
		     line = strtok_r(NULL, "\n", &save))
		{
			const char* argv[] = { "printer", line };
			ok &= freerdp_client_add_device_channel(st, 2, argv);
		}
		free(copy);
	}
	/* folder redirection (rdpdr drives), like FreeRDP's /drive:name,path */
	if (cfg->drives && cfg->drives[0])
	{
		char* copy = _strdup(cfg->drives);
		char* save = NULL;
		for (char* line = copy ? strtok_r(copy, "\n", &save) : NULL; ok && line;
		     line = strtok_r(NULL, "\n", &save))
		{
			char* tab = strchr(line, '\t');
			if (!tab)
				continue;
			*tab = '\0';
			const char* argv[] = { "drive", line, tab + 1 };
			ok &= freerdp_client_add_device_channel(st, 3, argv);
		}
		free(copy);
	}
	if (cfg->auto_reconnect)
	{
		ok &= freerdp_settings_set_bool(st, FreeRDP_AutoReconnectionEnabled, TRUE);
		ok &= freerdp_settings_set_uint32(st, FreeRDP_AutoReconnectMaxRetries, 5);
	}

	if (cfg->monitor_count > 1)
	{
		ok &= freerdp_settings_set_bool(st, FreeRDP_UseMultimon, TRUE);
		ok &= freerdp_settings_set_pointer_len(st, FreeRDP_MonitorDefArray, NULL,
		                                       cfg->monitor_count);
		ok &= freerdp_settings_set_uint32(st, FreeRDP_MonitorCount, cfg->monitor_count);
		/* Primary first, like FreeRDP's own clients (freerdp_settings_set_monitor_def_array_
		 * sorted); the rest keeps its order. */
		uint32_t primary = 0;
		for (uint32_t i = 0; i < cfg->monitor_count; i++)
			if (cfg->monitors[i].primary)
			{
				primary = i;
				break;
			}
		for (uint32_t i = 0; ok && i < cfg->monitor_count; i++)
		{
			rdpMonitor* m = freerdp_settings_get_pointer_array_writable(st, FreeRDP_MonitorDefArray, i);
			if (!m)
				return FALSE;
			const uint32_t k = i == 0 ? primary : (i <= primary ? i - 1 : i);
			const rdpc_monitor* src = &cfg->monitors[k];
			m->x = src->left;
			m->y = src->top;
			m->width = src->right - src->left;
			m->height = src->bottom - src->top;
			m->is_primary = src->primary ? TRUE : FALSE;
		}
	}
	return ok;
}

/* ---- session -------------------------------------------------------------------------- */

rdpc_session* rdpc_session_new(const rdpc_config* cfg, rdpc_event_fn fn, void* user)
{
	if (!cfg || !cfg->host)
		return NULL;
	if (cfg->log_level)
		WLog_SetStringLogLevel(WLog_GetRoot(), cfg->log_level);
	/* Own config directory (~/.config/rmc/freerdp: trusted server certificates, ...),
	 * separate from other FreeRDP clients on the machine. */
	static BOOL details_set = FALSE;
	if (!details_set)
		details_set = freerdp_setApplicationDetails("rmc", "freerdp", -1);
	rdpc_session* s = calloc(1, sizeof(*s));
	if (!s)
		return NULL;
	s->fn = fn;
	s->user = user;
	s->auth = cfg->auth;
	s->cert_tofu = cfg->cert_tofu;
	s->cert_prompt = cfg->cert_prompt;
	if (cfg->aad_helper)
	{
		const char* base = strrchr(cfg->aad_helper, '/');
		base = base ? base + 1 : cfg->aad_helper;
		if (strcmp(base, "rmc-login") == 0)
			s->own_login = _strdup(cfg->aad_helper);
	}
	s->auto_reconnect = cfg->auto_reconnect;
	InitializeCriticalSection(&s->lock);
	s->wake = CreateEvent(NULL, TRUE, FALSE, NULL);
	RDP_CLIENT_ENTRY_POINTS ep = { 0 };
	client_entry(&ep);
	s->context = freerdp_client_context_new(&ep);
	if (!s->wake || !s->context)
		goto fail;
	((rdpcContext*)s->context)->session = s;
	if (!apply_config(s->context->settings, cfg))
		goto fail;
	return s;
fail:
	rdpc_session_free(s);
	return NULL;
}

static int queue_cmd(rdpc_session* s, rdpc_cmd cmd)
{
	if (!s)
		return -1;
	EnterCriticalSection(&s->lock);
	if (s->queue_len == s->queue_cap)
	{
		size_t cap = s->queue_cap ? s->queue_cap * 2 : 64;
		rdpc_cmd* q = realloc(s->queue, cap * sizeof(*q));
		if (!q)
		{
			LeaveCriticalSection(&s->lock);
			return -1;
		}
		s->queue = q;
		s->queue_cap = cap;
	}
	s->queue[s->queue_len++] = cmd;
	SetEvent(s->wake);
	LeaveCriticalSection(&s->lock);
	return 0;
}

int rdpc_send_key(rdpc_session* s, uint32_t scancode, uint32_t extended, uint32_t down)
{
	rdpc_cmd c = { CMD_KEY, scancode, extended, down, 0, NULL, 0 };
	return queue_cmd(s, c);
}

int rdpc_send_mouse(rdpc_session* s, uint32_t flags, uint32_t x, uint32_t y)
{
	rdpc_cmd c = { CMD_MOUSE, flags, x, y, 0, NULL, 0 };
	return queue_cmd(s, c);
}

int rdpc_send_frame_ack(rdpc_session* s, uint32_t frame_id, uint32_t total_decoded,
                        uint32_t queue_depth)
{
	rdpc_cmd c = { CMD_ACK, frame_id, total_decoded, queue_depth, 0, NULL, 0 };
	return queue_cmd(s, c);
}

static int queue_blob(rdpc_session* s, enum cmd_type type, uint32_t a, const uint8_t* data,
                      size_t len)
{
	rdpc_cmd c = { 0 };
	c.type = type;
	c.a = a;
	if (len)
	{
		c.blob = malloc(len);
		if (!c.blob)
			return -1;
		memcpy(c.blob, data, len);
		c.blob_len = len;
	}
	const int rc = queue_cmd(s, c);
	if (rc != 0)
		free(c.blob);
	return rc;
}

int rdpc_clip_set_formats(rdpc_session* s, const uint8_t* list, size_t len)
{
	return queue_blob(s, CMD_CLIP_FORMATS, 0, list, len);
}

int rdpc_clip_request(rdpc_session* s, uint32_t format_id)
{
	return queue_blob(s, CMD_CLIP_REQUEST, format_id, NULL, 0);
}

int rdpc_clip_respond(rdpc_session* s, uint32_t ok, const uint8_t* data, size_t len)
{
	return queue_blob(s, CMD_CLIP_RESPONSE, ok ? 1 : 0, data, ok ? len : 0);
}

int rdpc_clip_file_request(rdpc_session* s, uint32_t stream, uint32_t index, uint32_t flags,
                           uint64_t position, uint32_t size)
{
	uint8_t pos[8];
	const uint32_t lo = (uint32_t)position, hi = (uint32_t)(position >> 32);
	memcpy(pos, &lo, 4);
	memcpy(pos + 4, &hi, 4);
	rdpc_cmd c = { 0 };
	c.type = CMD_CLIP_FILE_REQUEST;
	c.a = stream;
	c.b = index;
	c.c = flags;
	c.d = size;
	c.blob = malloc(8);
	if (!c.blob)
		return -1;
	memcpy(c.blob, pos, 8);
	c.blob_len = 8;
	const int rc = queue_cmd(s, c);
	if (rc != 0)
		free(c.blob);
	return rc;
}

int rdpc_clip_file_respond(rdpc_session* s, uint32_t stream, uint32_t ok, const uint8_t* data,
                           size_t len)
{
	rdpc_cmd c = { 0 };
	c.type = CMD_CLIP_FILE_RESPONSE;
	c.a = stream;
	c.b = ok ? 1 : 0;
	if (ok && len)
	{
		c.blob = malloc(len);
		if (!c.blob)
			return -1;
		memcpy(c.blob, data, len);
		c.blob_len = len;
	}
	const int rc = queue_cmd(s, c);
	if (rc != 0)
		free(c.blob);
	return rc;
}

/* Runs on the session thread. */
static BOOL add_device(rdpc_session* s, uint32_t type, const char* spec);

static BOOL drain_queue(rdpc_session* s)
{
	rdpc_cmd local[256];
	for (;;)
	{
		size_t n = 0;
		EnterCriticalSection(&s->lock);
		n = s->queue_len < 256 ? s->queue_len : 256;
		memcpy(local, s->queue, n * sizeof(rdpc_cmd));
		memmove(s->queue, s->queue + n, (s->queue_len - n) * sizeof(rdpc_cmd));
		s->queue_len -= n;
		if (s->queue_len == 0)
			ResetEvent(s->wake);
		LeaveCriticalSection(&s->lock);
		if (n == 0)
			return TRUE;
		rdpInput* input = s->context->input;
		for (size_t i = 0; i < n; i++)
		{
			const rdpc_cmd* c = &local[i];
			BOOL ok = TRUE;
			switch (c->type)
			{
				case CMD_KEY:
					ok = freerdp_input_send_keyboard_event_ex(
					    input, c->c ? TRUE : FALSE, FALSE, MAKE_RDP_SCANCODE(c->a, c->b ? TRUE : FALSE));
					break;
				case CMD_MOUSE:
					if (c->a & RDPC_MOUSE_EXTENDED)
						ok = freerdp_input_send_extended_mouse_event(
						    input, (UINT16)(c->a & 0xFFFF), (UINT16)c->b, (UINT16)c->c);
					else
						ok = freerdp_input_send_mouse_event(input, (UINT16)c->a, (UINT16)c->b,
						                                    (UINT16)c->c);
					break;
				case CMD_ACK:
					if (s->gfx && s->gfx->FrameAcknowledge)
					{
						RDPGFX_FRAME_ACKNOWLEDGE_PDU pdu = { 0 };
						pdu.frameId = c->a;
						pdu.totalFramesDecoded = c->b;
						pdu.queueDepth = c->c;
						ok = s->gfx->FrameAcknowledge(s->gfx, &pdu) == CHANNEL_RC_OK;
					}
					break;
				case CMD_CLIP_FORMATS:
					free(s->clip_formats);
					s->clip_formats = c->blob;
					s->clip_formats_len = c->blob_len;
					local[i].blob = NULL; /* kept for the next MonitorReady */
					ok = clip_send_format_list(s, s->clip_formats, s->clip_formats_len) ==
					     CHANNEL_RC_OK;
					break;
				case CMD_CLIP_REQUEST:
					if (s->clip && s->clip->ClientFormatDataRequest)
					{
						CLIPRDR_FORMAT_DATA_REQUEST req = { 0 };
						req.common.msgType = CB_FORMAT_DATA_REQUEST;
						req.requestedFormatId = c->a;
						ok = s->clip->ClientFormatDataRequest(s->clip, &req) == CHANNEL_RC_OK;
					}
					break;
				case CMD_CLIP_RESPONSE:
					if (s->clip && s->clip->ClientFormatDataResponse)
					{
						CLIPRDR_FORMAT_DATA_RESPONSE resp = { 0 };
						resp.common.msgType = CB_FORMAT_DATA_RESPONSE;
						resp.common.msgFlags = c->a ? CB_RESPONSE_OK : CB_RESPONSE_FAIL;
						resp.common.dataLen = c->a ? (UINT32)c->blob_len : 0;
						resp.requestedFormatData = c->a ? c->blob : NULL;
						ok = s->clip->ClientFormatDataResponse(s->clip, &resp) == CHANNEL_RC_OK;
					}
					break;
				case CMD_CLIP_FILE_REQUEST:
					if (s->clip && s->clip->ClientFileContentsRequest && c->blob_len >= 8)
					{
						CLIPRDR_FILE_CONTENTS_REQUEST req = { 0 };
						req.common.msgType = CB_FILECONTENTS_REQUEST;
						req.streamId = c->a;
						req.listIndex = c->b;
						req.dwFlags = c->c;
						req.cbRequested = c->d;
						memcpy(&req.nPositionLow, c->blob, 4);
						memcpy(&req.nPositionHigh, c->blob + 4, 4);
						ok = s->clip->ClientFileContentsRequest(s->clip, &req) == CHANNEL_RC_OK;
					}
					break;
				case CMD_CLIP_FILE_RESPONSE:
					if (s->clip && s->clip->ClientFileContentsResponse)
					{
						CLIPRDR_FILE_CONTENTS_RESPONSE resp = { 0 };
						resp.common.msgType = CB_FILECONTENTS_RESPONSE;
						resp.common.msgFlags = c->b ? CB_RESPONSE_OK : CB_RESPONSE_FAIL;
						resp.streamId = c->a;
						resp.cbRequested = c->b ? (UINT32)c->blob_len : 0;
						resp.requestedData = c->b ? c->blob : NULL;
						ok = s->clip->ClientFileContentsResponse(s->clip, &resp) == CHANNEL_RC_OK;
					}
					break;
				case CMD_ADD_DEVICE:
					ok = add_device(s, c->a, (const char*)c->blob);
					break;
				default:
					break;
			}
			free(local[i].blob);
			if (!ok)
				emit_simple(s, RDPC_EV_ERROR, c->type, "failed to send input or acknowledgement");
		}
	}
}

/* Registers a folder (RDPDR_DTYP_FILESYSTEM, spec "name\tpath") or printer
 * (RDPDR_DTYP_PRINT, spec "name") with the running rdpdr channel; the server sees a device
 * announce. Runs on the session thread. */
static BOOL add_device(rdpc_session* s, uint32_t type, const char* spec)
{
	char msg[600];
	RdpdrClientContext* rdpdr =
	    s->context ? (RdpdrClientContext*)freerdp_channels_get_static_channel_interface(
	                     s->context->channels, RDPDR_SVC_CHANNEL_NAME)
	               : NULL;
	if (!rdpdr || !rdpdr->RdpdrRegisterDevice || !spec)
	{
		emit_simple(s, RDPC_EV_ERROR, 0, "device redirection channel not available");
		return TRUE;
	}
	char* copy = _strdup(spec);
	if (!copy)
		return FALSE;
	char* tab = strchr(copy, '\t');
	if (tab)
		*tab = '\0';
	const char* args[2] = { copy, tab ? tab + 1 : NULL };
	const size_t n = (type == RDPDR_DTYP_FILESYSTEM) ? 2 : 1;
	RDPDR_DEVICE* dev = freerdp_device_new(type, n, args);
	uint32_t id = 0;
	const UINT rc = dev ? rdpdr->RdpdrRegisterDevice(rdpdr, dev, &id) : ERROR_INVALID_DATA;
	snprintf(msg, sizeof(msg), "%s \"%s\" %s (device %" PRIu32 ", rc 0x%08X)",
	         type == RDPDR_DTYP_FILESYSTEM ? "drive" : "printer", copy,
	         rc == CHANNEL_RC_OK ? "added" : "NOT added", id, rc);
	emit_simple(s, rc == CHANNEL_RC_OK ? RDPC_EV_INFO : RDPC_EV_ERROR, 0, msg);
	freerdp_device_free(dev);
	free(copy);
	return TRUE;
}

int rdpc_add_device(rdpc_session* s, uint32_t type, const char* spec)
{
	if (!spec)
		return -1;
	rdpc_cmd c = { 0 };
	c.type = CMD_ADD_DEVICE;
	c.a = type;
	c.blob = (uint8_t*)_strdup(spec);
	if (!c.blob)
		return -1;
	c.blob_len = strlen(spec) + 1;
	const int rc = queue_cmd(s, c);
	if (rc != 0)
		free(c.blob);
	return rc;
}

int rdpc_session_run(rdpc_session* s)
{
	if (!s)
		return -1;
	freerdp* instance = s->context->instance;
	if (freerdp_client_start(s->context) != 0)
	{
		emit_simple(s, RDPC_EV_DISCONNECTED, 0, "freerdp_client_start failed");
		return -1;
	}
	int result = 0;
	if (!freerdp_connect(instance))
	{
		const UINT32 err = freerdp_get_last_error(s->context);
		char msg[256];
		snprintf(msg, sizeof(msg), "connection failed: %s (0x%08x)",
		         freerdp_get_last_error_string(err), err);
		emit_simple(s, RDPC_EV_DISCONNECTED, err, msg);
		freerdp_client_stop(s->context);
		return -1;
	}
	HANDLE handles[MAXIMUM_WAIT_OBJECTS] = { 0 };
	while (!freerdp_shall_disconnect_context(s->context) && !InterlockedCompareExchange(&s->stop, 0, 0))
	{
		DWORD n = freerdp_get_event_handles(s->context, handles, ARRAYSIZE(handles) - 1);
		if (n == 0)
		{
			result = -1;
			break;
		}
		handles[n++] = s->wake;
		const DWORD status = WaitForMultipleObjects(n, handles, FALSE, INFINITE);
		if (status == WAIT_FAILED)
		{
			result = -1;
			break;
		}
		if (InterlockedCompareExchange(&s->stop, 0, 0))
			break;
		if (!drain_queue(s))
		{
			result = -1;
			break;
		}
		if (!freerdp_check_event_handles(s->context))
		{
			/* Network drop: FreeRDP's auto-reconnect (ARC cookie, same credentials; up to
			 * AutoReconnectMaxRetries). The graphics channel is re-created afterwards. */
			if (s->auto_reconnect && !InterlockedCompareExchange(&s->stop, 0, 0))
			{
				emit_simple(s, RDPC_EV_INFO, 0, "connection lost, trying to reconnect");
				if (client_auto_reconnect_ex(instance, NULL))
				{
					emit_simple(s, RDPC_EV_INFO, 0, "reconnected");
					continue;
				}
			}
			if (freerdp_get_last_error(s->context) != FREERDP_ERROR_SUCCESS)
				result = -1;
			break;
		}
	}
	const UINT32 err = freerdp_get_last_error(s->context);
	freerdp_disconnect(instance);
	freerdp_client_stop(s->context);
	char msg[256];
	snprintf(msg, sizeof(msg), "disconnected: %s (0x%08x)", freerdp_get_last_error_string(err),
	         err);
	emit_simple(s, RDPC_EV_DISCONNECTED, err, msg);
	return result;
}

void rdpc_session_stop(rdpc_session* s)
{
	if (!s)
		return;
	InterlockedExchange(&s->stop, 1);
	if (s->wake)
		SetEvent(s->wake);
}

void rdpc_session_free(rdpc_session* s)
{
	if (!s)
		return;
	if (s->context)
		freerdp_client_context_free(s->context);
	if (s->codecs)
		freerdp_client_codecs_free(s->codecs);
	if (s->wake)
		CloseHandle(s->wake);
	DeleteCriticalSection(&s->lock);
	free(s->queue);
	free(s->pixbuf);
	free(s->own_login);
	free(s->clip_formats);
	for (size_t i = 0; i < s->queue_len; i++)
		free(s->queue[i].blob);
	free(s);
}
