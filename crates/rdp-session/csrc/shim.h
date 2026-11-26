/* Thin C layer between libfreerdp 3 and the Rust client (crate rdp-session).
 *
 * It owns the FreeRDP instance, settings, channel callbacks and event loop and hands the
 * graphics pipeline (RDPEGFX) to Rust as small tagged events. Struct layouts and macros
 * of FreeRDP stay on the C side. Input and frame acknowledgements are queued and sent from
 * the session thread, so Rust never calls into libfreerdp from another thread.
 */
#ifndef RDPC_SHIM_H
#define RDPC_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct rdpc_session rdpc_session;

/* Same layout as RECTANGLE_16 / RDPGFX_POINT16. */
typedef struct
{
	uint16_t left, top, right, bottom;
} rdpc_rect;

typedef struct
{
	uint16_t x, y;
} rdpc_point;

/* Monitor in virtual desktop coordinates (exclusive right/bottom). */
typedef struct
{
	int32_t left, top, right, bottom;
	uint32_t primary;
} rdpc_monitor;

enum rdpc_auth
{
	RDPC_AUTH_PASSWORD = 0,
	RDPC_AUTH_AAD = 1 /* Entra ID (RDS AAD): token via the out-of-process login helper */
};

typedef struct
{
	const char* host;
	uint16_t port;
	const char* username;
	const char* domain;
	const char* password;
	uint32_t auth;
	uint32_t width, height;
	const rdpc_monitor* monitors;
	uint32_t monitor_count;
	uint32_t avc444;             /* advertise AVC444 (v1 and v2) besides AVC420 */
	uint32_t ignore_certificate; /* local test servers only */
	const char* log_level;       /* WLog level (e.g. "WARN"), NULL keeps the default */
	const char* aad_helper;      /* path of freerdp-xdg-aad-helper / -webview- (AAD only) */
	uint32_t cert_tofu;          /* unknown certificate: accept and store (trust on first use);
	                                a changed certificate is always rejected */
	const char* sound;           /* rdpsnd arguments, e.g. "sys:pulse"; NULL = no playback */
	const char* microphone;      /* audin arguments, e.g. "sys:pulse"; NULL = no microphone */
	const char* camera;          /* rdpecam arguments ("" = default v4l); NULL = no camera */
	uint32_t auto_reconnect;     /* reconnect automatically after a network drop (5 tries) */
	uint32_t clipboard;          /* clipboard channel (cliprdr) */
	const char* drives;          /* redirected folders: "name\tpath\n" lines; NULL = none */
	const char* printers;        /* redirected CUPS printers: "name\n" lines; NULL = none */
	uint32_t cert_prompt;        /* unknown or changed certificate: ask on stdout/stdin
	                                (line "RDPC-CERT\t...", answer "yes"/"no"); overrides
	                                cert_tofu. Entra renewals stay automatic. */
} rdpc_config;

enum rdpc_event_kind
{
	RDPC_EV_CONNECTED = 1,      /* a, b = desktop size */
	RDPC_EV_DISCONNECTED,       /* a = FreeRDP last error, message */
	RDPC_EV_ERROR,              /* message */
	RDPC_EV_CAPS_CONFIRM,       /* a = version, b = flags */
	RDPC_EV_RESET_GRAPHICS,     /* a, b = size, monitors */
	RDPC_EV_CREATE_SURFACE,     /* surface, a, b = size, c = pixel format (0x20/0x21) */
	RDPC_EV_DELETE_SURFACE,     /* surface */
	RDPC_EV_MAP_SURFACE,        /* surface, a, b = output origin */
	RDPC_EV_START_FRAME,        /* a = frame id, b = timestamp */
	RDPC_EV_END_FRAME,          /* a = frame id */
	RDPC_EV_SOLID_FILL,         /* surface, a = colour (B | G<<8 | R<<16 | XA<<24), rects */
	RDPC_EV_SURFACE_TO_SURFACE, /* surface = src, surface2 = dst, rect = source, points */
	RDPC_EV_SURFACE_TO_CACHE,   /* surface, rect, a = slot */
	RDPC_EV_CACHE_TO_SURFACE,   /* a = slot, surface, points */
	RDPC_EV_EVICT_CACHE,        /* a = slot */
	RDPC_EV_AVC420,             /* surface, rect = destination, rects + data = main view */
	RDPC_EV_AVC444,             /* surface, a = 1 (v1) / 2 (v2), b = LC, rect = destination,
	                               rects + data = main view, rects2 + data2 = aux view
	                               (data / data2 NULL when LC leaves the view out) */
	RDPC_EV_PIXELS,             /* surface, rect = destination, data = tight BGRA rows */
	RDPC_EV_UNSUPPORTED_CODEC,  /* surface, a = codec id, rect */
	RDPC_EV_POINTER_SET,        /* a = cache index, b, c = size, d = hot x | hot y << 16,
	                               data = tight BGRA rows */
	RDPC_EV_POINTER_CACHED,     /* a = cache index */
	RDPC_EV_POINTER_SYSTEM,     /* a = 0: hidden, 1: default arrow */
	RDPC_EV_INFO,               /* message (certificate accepted, channels, ...) */
	RDPC_EV_GFX_CLOSED,         /* graphics channel closed (e.g. before an auto-reconnect):
	                               all surfaces and cache entries are gone */
	RDPC_EV_CLIP_READY,         /* clipboard channel ready (formats re-announced) */
	RDPC_EV_CLIP_REMOTE_FORMATS, /* server clipboard changed: data = "id\tname\n" lines */
	RDPC_EV_CLIP_DATA_REQUEST,  /* server pastes: a = requested format id */
	RDPC_EV_CLIP_DATA,          /* answer to rdpc_clip_request: a = ok, data */
	RDPC_EV_CLIP_FILE_REQUEST,  /* server reads a file: a = stream, b = list index, c = flags
	                               (1 size, 2 range), d = bytes, surface/surface2 = position
	                               low/high */
	RDPC_EV_CLIP_FILE_DATA      /* answer to rdpc_clip_file_request: a = stream, b = ok, data */
};

/* Pointers are valid only during the callback. */
typedef struct
{
	uint32_t kind;
	uint32_t surface;
	uint32_t surface2;
	uint32_t a, b, c, d;
	rdpc_rect rect;
	const rdpc_rect* rects;
	uint32_t rect_count;
	const rdpc_rect* rects2;
	uint32_t rect_count2;
	const rdpc_point* points;
	uint32_t point_count;
	const uint8_t* data;
	uint32_t length;
	const uint8_t* data2;
	uint32_t length2;
	const rdpc_monitor* monitors;
	uint32_t monitor_count;
	const char* message;
} rdpc_event;

typedef void (*rdpc_event_fn)(void* user, const rdpc_event* ev);

/* Mouse flags as in MS-RDPBCGR (PTR_FLAGS_*); RDPC_MOUSE_EXTENDED selects the extended
 * mouse event (PTR_XFLAGS_* for buttons 4/5). */
#define RDPC_MOUSE_EXTENDED 0x80000000u

rdpc_session* rdpc_session_new(const rdpc_config* cfg, rdpc_event_fn fn, void* user);
/* Connects and runs the event loop until disconnected or stopped (blocking). 0 = clean. */
int rdpc_session_run(rdpc_session* s);
/* Thread-safe: ends the session loop. */
void rdpc_session_stop(rdpc_session* s);
void rdpc_session_free(rdpc_session* s);

/* Thread-safe, queued to the session thread. Return 0 on success. */
int rdpc_send_key(rdpc_session* s, uint32_t scancode, uint32_t extended, uint32_t down);
int rdpc_send_mouse(rdpc_session* s, uint32_t flags, uint32_t x, uint32_t y);
/* Synchronize event: state of the lock keys (KBD_SYNC_SCROLL_LOCK 1, NUM_LOCK 2, CAPS_LOCK 4). */
int rdpc_send_sync(rdpc_session* s, uint32_t flags);
int rdpc_send_frame_ack(rdpc_session* s, uint32_t frame_id, uint32_t total_decoded,
                        uint32_t queue_depth);
/* Clipboard: announce local formats ("id\tname\n" lines), request server data for a format
 * (answer: RDPC_EV_CLIP_DATA), answer a server request (RDPC_EV_CLIP_DATA_REQUEST). */
int rdpc_clip_set_formats(rdpc_session* s, const uint8_t* list, size_t len);
int rdpc_clip_request(rdpc_session* s, uint32_t format_id);
int rdpc_clip_respond(rdpc_session* s, uint32_t ok, const uint8_t* data, size_t len);
/* File contents (FILECONTENTS_SIZE = 1, FILECONTENTS_RANGE = 2). */
int rdpc_clip_file_request(rdpc_session* s, uint32_t stream, uint32_t index, uint32_t flags,
                           uint64_t position, uint32_t size);
int rdpc_clip_file_respond(rdpc_session* s, uint32_t stream, uint32_t ok, const uint8_t* data,
                           size_t len);

/* Adds a device during the session: type 8 = folder ("name\tpath"), 4 = printer ("name").
 * The result comes as an RDPC_EV_INFO/ERROR event. */
int rdpc_add_device(rdpc_session* s, uint32_t type, const char* spec);

#ifdef __cplusplus
}
#endif

#endif
