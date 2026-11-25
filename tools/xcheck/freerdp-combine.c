/* Cross-check helper: combines AVC444 views with FreeRDP's own primitives.
 *
 *   freerdp-combine <width> <height> <v1|v2>
 *
 * stdin:  pairs of I420 pictures (main view, auxiliary view), padded size width x height
 * stdout: for each pair the combined YUV 4:4:4 planes (Y, U, V; width x height each)
 *
 * Uses primitives_get()->YUV420CombineToYUV444 exactly like libfreerdp/codec/h264.c:
 * AVC444_LUMA for the main view, AVC444_CHROMAv1/v2 for the auxiliary view.
 *
 * Build: see tools/xcheck/build.sh
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <freerdp/primitives.h>

static int read_all(void* buf, size_t len)
{
	return fread(buf, 1, len, stdin) == len;
}

int main(int argc, char** argv)
{
	if (argc != 4)
	{
		fprintf(stderr, "usage: %s <width> <height> <v1|v2>\n", argv[0]);
		return 2;
	}
	const UINT32 w = (UINT32)strtoul(argv[1], NULL, 10);
	const UINT32 h = (UINT32)strtoul(argv[2], NULL, 10);
	const avc444_frame_type chroma = strcmp(argv[3], "v2") == 0 ? AVC444_CHROMAv2 : AVC444_CHROMAv1;
	const size_t n = (size_t)w * h;

	primitives_t* prims = primitives_get();
	if (!prims || !prims->YUV420CombineToYUV444)
	{
		fprintf(stderr, "no YUV420CombineToYUV444 primitive\n");
		return 1;
	}

	BYTE* in = malloc(n * 3 / 2);
	BYTE* out = malloc(n * 3);
	if (!in || !out)
		return 1;
	memset(out, 0, n);
	memset(out + n, 128, 2 * n);

	BYTE* dst[3] = { out, out + n, out + 2 * n };
	const UINT32 dstStep[3] = { w, w, w };
	const UINT32 srcStep[3] = { w, w / 2, w / 2 };
	const RECTANGLE_16 roi = { 0, 0, (UINT16)w, (UINT16)h };

	for (;;)
	{
		const BYTE* src[3] = { in, in + n, in + n + n / 4 };
		if (!read_all(in, n * 3 / 2))
			break;
		if (prims->YUV420CombineToYUV444(AVC444_LUMA, src, srcStep, w, h, dst, dstStep, &roi) != 0)
			return 1;
		if (!read_all(in, n * 3 / 2))
		{
			fprintf(stderr, "missing auxiliary view\n");
			return 1;
		}
		if (prims->YUV420CombineToYUV444(chroma, src, srcStep, w, h, dst, dstStep, &roi) != 0)
			return 1;
		if (fwrite(out, 1, n * 3, stdout) != n * 3)
			return 1;
	}
	free(in);
	free(out);
	return 0;
}
