/* Cross-check helper: splits an RGB picture into AVC444 views with FreeRDP's encoder primitive
 * (the one the shadow server uses).
 *
 *   freerdp-split <in.ppm> <v1|v2> <opt|generic>
 *
 * stdout: main view I420, then auxiliary view I420 (padded to 16x16, like the encoder)
 * opt     = primitives_get() (SIMD where available), generic = primitives_get_generic()
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <freerdp/codec/color.h>
#include <freerdp/primitives.h>

static BYTE* read_ppm(const char* path, UINT32* w, UINT32* h)
{
	FILE* f = fopen(path, "rb");
	if (!f)
		return NULL;
	unsigned maxv = 0;
	if (fscanf(f, "P6 %u %u %u", w, h, &maxv) != 3 || maxv != 255)
	{
		fclose(f);
		return NULL;
	}
	fgetc(f);
	const size_t n = (size_t)*w * *h;
	BYTE* rgb = malloc(n * 3);
	BYTE* bgrx = malloc(n * 4);
	if (!rgb || !bgrx || fread(rgb, 1, n * 3, f) != n * 3)
	{
		fclose(f);
		return NULL;
	}
	fclose(f);
	for (size_t i = 0; i < n; i++)
	{
		bgrx[4 * i + 0] = rgb[3 * i + 2];
		bgrx[4 * i + 1] = rgb[3 * i + 1];
		bgrx[4 * i + 2] = rgb[3 * i + 0];
		bgrx[4 * i + 3] = 0xFF;
	}
	free(rgb);
	return bgrx;
}

int main(int argc, char** argv)
{
	if (argc != 4)
	{
		fprintf(stderr, "usage: %s <in.ppm> <v1|v2> <opt|generic>\n", argv[0]);
		return 2;
	}
	UINT32 w = 0, h = 0;
	BYTE* src = read_ppm(argv[1], &w, &h);
	if (!src)
	{
		fprintf(stderr, "cannot read %s\n", argv[1]);
		return 1;
	}
	const BOOL v2 = strcmp(argv[2], "v2") == 0;
	primitives_t* prims = strcmp(argv[3], "generic") == 0 ? primitives_get_generic() : primitives_get();
	const UINT32 pw = (w + 15) & ~15u, ph = (h + 15) & ~15u;
	const size_t n = (size_t)pw * ph;
	BYTE* main_ = calloc(1, n * 3 / 2);
	BYTE* aux = calloc(1, n * 3 / 2);
	BYTE* pMain[3] = { main_, main_ + n, main_ + n + n / 4 };
	BYTE* pAux[3] = { aux, aux + n, aux + n + n / 4 };
	const UINT32 step[3] = { pw, pw / 2, pw / 2 };
	const prim_size_t roi = { w, h };
	fn_RGBToAVC444YUV_t fn = v2 ? prims->RGBToAVC444YUVv2 : prims->RGBToAVC444YUV;
	if (fn(src, PIXEL_FORMAT_BGRX32, w * 4, pMain, step, pAux, step, &roi) != 0)
	{
		fprintf(stderr, "RGBToAVC444YUV%s failed\n", v2 ? "v2" : "");
		return 1;
	}
	fwrite(main_, 1, n * 3 / 2, stdout);
	fwrite(aux, 1, n * 3 / 2, stdout);
	fprintf(stderr, "%ux%u (padded %ux%u), %s, %s\n", w, h, pw, ph, v2 ? "v2" : "v1", argv[3]);
	return 0;
}
