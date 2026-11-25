/* freerdp-rec: replays a recording of rmc (--record) through FreeRDP's own AVC
 * decoder (avc444_decompress / avc420_decompress, software or its configured hwaccel) and
 * writes the surfaces as PPM every N frames: <out>/freerdp-<surface>-<frame>.ppm.
 * Reference for tools comparing the GPU core against FreeRDP on real server streams.
 *
 *   freerdp-rec <recording dir> <out dir> [every N frames, default 50] [max frames]
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <winpr/json.h>
#include <freerdp/codec/color.h>
#include <freerdp/codec/h264.h>
#include <freerdp/channels/rdpgfx.h>

#define MAXS 64

typedef struct
{
	int used;
	UINT32 w, h;
	BYTE* px; /* BGRA */
	H264_CONTEXT* h264;
	FILE* stream;
} Surf;

static Surf surf[MAXS];

static BYTE* read_au(Surf* s, const WINPR_JSON* au, UINT32* len)
{
	const UINT64 off = (UINT64)WINPR_JSON_GetNumberValue(WINPR_JSON_GetObjectItem(au, "offset"));
	*len = (UINT32)WINPR_JSON_GetNumberValue(WINPR_JSON_GetObjectItem(au, "len"));
	BYTE* buf = malloc(*len ? *len : 1);
	if (fseeko(s->stream, (off_t)off, SEEK_SET) != 0 || fread(buf, 1, *len, s->stream) != *len)
	{
		free(buf);
		return NULL;
	}
	return buf;
}

static RECTANGLE_16* read_rects(const WINPR_JSON* arr, UINT32* n)
{
	*n = (UINT32)WINPR_JSON_GetArraySize(arr);
	RECTANGLE_16* r = calloc(*n ? *n : 1, sizeof(RECTANGLE_16));
	for (UINT32 i = 0; i < *n; i++)
	{
		const WINPR_JSON* a = WINPR_JSON_GetArrayItem(arr, i);
		r[i].left = (UINT16)WINPR_JSON_GetNumberValue(WINPR_JSON_GetArrayItem(a, 0));
		r[i].top = (UINT16)WINPR_JSON_GetNumberValue(WINPR_JSON_GetArrayItem(a, 1));
		r[i].right = (UINT16)WINPR_JSON_GetNumberValue(WINPR_JSON_GetArrayItem(a, 2));
		r[i].bottom = (UINT16)WINPR_JSON_GetNumberValue(WINPR_JSON_GetArrayItem(a, 3));
	}
	return r;
}

static void dump(const char* out, int id, UINT32 frame)
{
	Surf* s = &surf[id];
	char path[1024];
	snprintf(path, sizeof(path), "%s/freerdp-%d-%06u.ppm", out, id, frame);
	FILE* f = fopen(path, "wb");
	if (!f)
		return;
	fprintf(f, "P6\n%u %u\n255\n", s->w, s->h);
	for (size_t i = 0; i < (size_t)s->w * s->h; i++)
	{
		const BYTE rgb[3] = { s->px[i * 4 + 2], s->px[i * 4 + 1], s->px[i * 4 + 0] };
		fwrite(rgb, 1, 3, f);
	}
	fclose(f);
}

int main(int argc, char** argv)
{
	if (argc < 3)
	{
		fprintf(stderr, "usage: %s <recording dir> <out dir> [every N frames]\n", argv[0]);
		return 2;
	}
	const char* dir = argv[1];
	const char* out = argv[2];
	const UINT32 every = argc > 3 ? (UINT32)atoi(argv[3]) : 50;
	const UINT32 maxframes = argc > 4 ? (UINT32)atoi(argv[4]) : 0xFFFFFFFFu;
	char path[1024];
	snprintf(path, sizeof(path), "%s/commands.jsonl", dir);
	FILE* log = fopen(path, "r");
	if (!log)
	{
		perror(path);
		return 1;
	}
	char* line = NULL;
	size_t cap = 0;
	UINT32 frames = 0, avc = 0, errors = 0;
	while (getline(&line, &cap, log) > 0)
	{
		WINPR_JSON* j = WINPR_JSON_Parse(line);
		if (!j)
			continue;
		const char* cmd = WINPR_JSON_GetStringValue(WINPR_JSON_GetObjectItem(j, "cmd"));
		const int id = (int)WINPR_JSON_GetNumberValue(WINPR_JSON_GetObjectItem(j, "id"));
		if (!cmd)
			;
		else if (strcmp(cmd, "create") == 0 && id < MAXS)
		{
			Surf* s = &surf[id];
			s->w = (UINT32)WINPR_JSON_GetNumberValue(WINPR_JSON_GetObjectItem(j, "width"));
			s->h = (UINT32)WINPR_JSON_GetNumberValue(WINPR_JSON_GetObjectItem(j, "height"));
			free(s->px);
			s->px = calloc((size_t)s->w * s->h, 4);
			for (size_t i = 0; i < (size_t)s->w * s->h; i++)
				s->px[i * 4 + 3] = 0xFF;
			if (s->h264)
				h264_context_free(s->h264);
			s->h264 = h264_context_new(FALSE);
			if (!s->h264 || !h264_context_reset(s->h264, s->w, s->h))
				return 1;
			if (!s->stream)
			{
				snprintf(path, sizeof(path), "%s/surface-%d.h264", dir, id);
				s->stream = fopen(path, "rb");
			}
			s->used = 1;
		}
		else if (strcmp(cmd, "delete") == 0 && id < MAXS && surf[id].used)
		{
			/* The stream file continues (offsets are global per surface id). */
			h264_context_free(surf[id].h264);
			surf[id].h264 = NULL;
			surf[id].used = 0;
		}
		else if ((strcmp(cmd, "avc444") == 0 || strcmp(cmd, "avc420") == 0) && id < MAXS &&
		         surf[id].used && surf[id].stream)
		{
			Surf* s = &surf[id];
			const WINPR_JSON* m = WINPR_JSON_GetObjectItem(j, "main");
			const WINPR_JSON* a = WINPR_JSON_GetObjectItem(j, "aux");
			const BOOL hasM = m && !WINPR_JSON_IsNull(m);
			const BOOL hasA = a && !WINPR_JSON_IsNull(a);
			UINT32 nm = 0, na = 0, lm = 0, la = 0;
			RECTANGLE_16* rm = hasM ? read_rects(WINPR_JSON_GetObjectItem(m, "rects"), &nm) : NULL;
			RECTANGLE_16* ra = hasA ? read_rects(WINPR_JSON_GetObjectItem(a, "rects"), &na) : NULL;
			BYTE* dm = hasM ? read_au(s, WINPR_JSON_GetObjectItem(m, "au"), &lm) : NULL;
			BYTE* da = hasA ? read_au(s, WINPR_JSON_GetObjectItem(a, "au"), &la) : NULL;
			INT32 rc = 0;
			if (strcmp(cmd, "avc420") == 0)
				rc = avc420_decompress(s->h264, dm, lm, s->px, PIXEL_FORMAT_BGRA32, s->w * 4, s->w,
				                       s->h, rm, nm);
			else
			{
				const BOOL v2 = WINPR_JSON_IsTrue(WINPR_JSON_GetObjectItem(j, "v2"));
				const UINT32 codec = v2 ? RDPGFX_CODECID_AVC444v2 : RDPGFX_CODECID_AVC444;
				/* As gdi/gfx.c: stream 1 = main (LC 0/1) or chroma (LC 2), stream 2 = aux. */
				if (hasM && hasA)
					rc = avc444_decompress(s->h264, 0, rm, nm, dm, lm, ra, na, da, la, s->px,
					                       PIXEL_FORMAT_BGRA32, s->w * 4, s->w, s->h, codec);
				else if (hasM)
					rc = avc444_decompress(s->h264, 1, rm, nm, dm, lm, NULL, 0, NULL, 0, s->px,
					                       PIXEL_FORMAT_BGRA32, s->w * 4, s->w, s->h, codec);
				else if (hasA)
					rc = avc444_decompress(s->h264, 2, ra, na, da, la, NULL, 0, NULL, 0, s->px,
					                       PIXEL_FORMAT_BGRA32, s->w * 4, s->w, s->h, codec);
			}
			if (rc < 0)
				errors++;
			avc++;
			free(rm);
			free(ra);
			free(dm);
			free(da);
		}
		else if (strcmp(cmd, "end") == 0)
		{
			frames++;
			if (frames % every == 0)
				for (int i = 0; i < MAXS; i++)
					if (surf[i].used)
						dump(out, i, frames);
		}
		WINPR_JSON_Delete(j);
		if (frames >= maxframes)
			break;
	}
	for (int i = 0; i < MAXS; i++)
		if (surf[i].used)
			dump(out, i, frames);
	printf("freerdp-rec: %u Frames, %u AVC-Updates, %u Fehler\n", frames, avc, errors);
	free(line);
	return 0;
}
