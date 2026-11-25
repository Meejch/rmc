/* xprobe: input probe for the shadow-server end-to-end test.
 *
 * Covers the whole X screen with one window (optionally showing a binary PPM as
 * background), takes the keyboard focus and logs every key, button and pointer event it
 * receives, one line each on stdout:
 *   READY 3440x1440
 *   KEY press keycode=27 keysym=r
 *   BUTTON press 1 at 100,100
 *   MOTION 100,100            (only when the position changed)
 * Button presses also paint a small marker, visible in the RDP picture, and switch the
 * window cursor between two shapes (the shadow server then sends a new pointer shape).
 *
 *   DISPLAY=:77 xprobe [background.ppm]
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static unsigned char* load_ppm(const char* path, int* w, int* h)
{
	FILE* f = fopen(path, "rb");
	if (!f)
		return NULL;
	int maxv = 0;
	if (fscanf(f, "P6 %d %d %d", w, h, &maxv) != 3 || maxv != 255)
	{
		fclose(f);
		return NULL;
	}
	fgetc(f);
	size_t n = (size_t)(*w) * (size_t)(*h) * 3;
	unsigned char* rgb = malloc(n);
	if (!rgb || fread(rgb, 1, n, f) != n)
	{
		free(rgb);
		fclose(f);
		return NULL;
	}
	fclose(f);
	return rgb;
}

int main(int argc, char** argv)
{
	Display* d = XOpenDisplay(NULL);
	if (!d)
	{
		fprintf(stderr, "xprobe: cannot open display\n");
		return 1;
	}
	int scr = DefaultScreen(d);
	int sw = DisplayWidth(d, scr), sh = DisplayHeight(d, scr);
	XSetWindowAttributes a;
	memset(&a, 0, sizeof(a));
	a.override_redirect = True;
	a.background_pixel = BlackPixel(d, scr);
	a.event_mask = KeyPressMask | KeyReleaseMask | ButtonPressMask | ButtonReleaseMask |
	               PointerMotionMask | ExposureMask;
	Window win = XCreateWindow(d, RootWindow(d, scr), 0, 0, (unsigned)sw, (unsigned)sh, 0,
	                           CopyFromParent, InputOutput, CopyFromParent,
	                           CWOverrideRedirect | CWBackPixel | CWEventMask, &a);
	XMapRaised(d, win);
	GC gc = XCreateGC(d, win, 0, NULL);

	/* Two cursors from bitmaps (no cursor font needed on Xvfb): a cross and a square. */
	Cursor cursors[2];
	for (int c = 0; c < 2; c++)
	{
		char bits[32] = { 0 }, mask[32];
		for (int y = 0; y < 16; y++)
			for (int x = 0; x < 16; x++)
			{
				int on = c == 0 ? (x == 7 || y == 7) : (x == 0 || y == 0 || x == 15 || y == 15);
				if (on)
					bits[y * 2 + x / 8] |= (char)(1 << (x % 8));
			}
		memset(mask, 0xFF, sizeof(mask));
		Pixmap pb = XCreateBitmapFromData(d, win, bits, 16, 16);
		Pixmap pm = XCreateBitmapFromData(d, win, mask, 16, 16);
		XColor fg = { 0 }, bg = { 0 };
		fg.red = fg.green = 0xFFFF;
		fg.flags = bg.flags = DoRed | DoGreen | DoBlue;
		cursors[c] = XCreatePixmapCursor(d, pb, pm, &fg, &bg, 7, 7);
	}
	int next_cursor = 0;

	XImage* img = NULL;
	if (argc > 1)
	{
		int w = 0, h = 0;
		unsigned char* rgb = load_ppm(argv[1], &w, &h);
		if (!rgb)
		{
			fprintf(stderr, "xprobe: cannot read %s\n", argv[1]);
			return 1;
		}
		Visual* vis = DefaultVisual(d, scr);
		char* px = malloc((size_t)w * (size_t)h * 4);
		for (size_t i = 0; i < (size_t)w * (size_t)h; i++)
		{
			px[i * 4 + 0] = (char)rgb[i * 3 + 2];
			px[i * 4 + 1] = (char)rgb[i * 3 + 1];
			px[i * 4 + 2] = (char)rgb[i * 3 + 0];
			px[i * 4 + 3] = 0;
		}
		free(rgb);
		img = XCreateImage(d, vis, (unsigned)DefaultDepth(d, scr), ZPixmap, 0, px, (unsigned)w,
		                   (unsigned)h, 32, 0);
	}

	/* Wait until mapped, then take the keyboard focus. */
	XEvent ev;
	for (;;)
	{
		XNextEvent(d, &ev);
		if (ev.type == Expose)
			break;
	}
	if (img)
		XPutImage(d, win, gc, img, 0, 0, 0, 0, img->width, img->height);
	XSetInputFocus(d, win, RevertToParent, CurrentTime);
	XFlush(d);
	printf("READY %dx%d\n", sw, sh);
	fflush(stdout);

	int lx = -1, ly = -1;
	for (;;)
	{
		XNextEvent(d, &ev);
		switch (ev.type)
		{
			case Expose:
				if (img && ev.xexpose.count == 0)
					XPutImage(d, win, gc, img, 0, 0, 0, 0, img->width, img->height);
				break;
			case KeyPress:
			case KeyRelease:
			{
				KeySym ks = XLookupKeysym(&ev.xkey, (ev.xkey.state & ShiftMask) ? 1 : 0);
				const char* name = XKeysymToString(ks);
				printf("KEY %s keycode=%u keysym=%s\n", ev.type == KeyPress ? "press" : "release",
				       ev.xkey.keycode, name ? name : "?");
				break;
			}
			case ButtonPress:
			case ButtonRelease:
				printf("BUTTON %s %u at %d,%d\n", ev.type == ButtonPress ? "press" : "release",
				       ev.xbutton.button, ev.xbutton.x, ev.xbutton.y);
				if (ev.type == ButtonPress && ev.xbutton.button <= 3)
				{
					XSetForeground(d, gc, 0xFF2020);
					XFillRectangle(d, win, gc, ev.xbutton.x - 6, ev.xbutton.y - 6, 13, 13);
					XDefineCursor(d, win, cursors[next_cursor]);
					printf("CURSOR %d\n", next_cursor);
					next_cursor ^= 1;
				}
				break;
			case MotionNotify:
				if (ev.xmotion.x != lx || ev.xmotion.y != ly)
				{
					lx = ev.xmotion.x;
					ly = ev.xmotion.y;
					printf("MOTION %d,%d\n", lx, ly);
				}
				break;
			default:
				break;
		}
		fflush(stdout);
	}
}
