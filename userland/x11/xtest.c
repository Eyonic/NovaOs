/* xtest: an X11 client (Xlib) on Xwayland: opens a window, paints it, and reads the
 * pixels back from the server. */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>

int main(int argc, char **argv)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	int linger = argc > 1 ? atoi(argv[1]) : 0;
	Display *d = XOpenDisplay(NULL);
	if (!d) {
		printf("xtest: cannot open display %s\n", getenv("DISPLAY") ? getenv("DISPLAY") : "(unset)");
		return 1;
	}
	int s = DefaultScreen(d);
	printf("xtest: connected to %s, screen %dx%d, vendor %s\n", DisplayString(d), DisplayWidth(d, s), DisplayHeight(d, s), ServerVendor(d));
	Window w = XCreateSimpleWindow(d, RootWindow(d, s), 40, 40, 200, 150, 0, 0, 0x22AA44);
	XStoreName(d, w, "xtest");
	XSelectInput(d, w, ExposureMask | StructureNotifyMask);
	XMapWindow(d, w);
	XEvent e;
	int mapped = 0, exposed = 0;
	while (!mapped || !exposed) {
		XNextEvent(d, &e);
		mapped |= e.type == MapNotify;
		exposed |= e.type == Expose;
	}
	GC gc = XCreateGC(d, w, 0, NULL);
	XSetForeground(d, gc, 0xDD2222);
	XFillRectangle(d, w, gc, 80, 55, 40, 40);
	XSync(d, False);
	XImage *img = XGetImage(d, w, 0, 0, 200, 150, AllPlanes, ZPixmap);
	unsigned long corner = XGetPixel(img, 5, 5), center = XGetPixel(img, 100, 75);
	printf("xtest: window mapped and exposed; pixels corner #%06lx center #%06lx\n", corner & 0xFFFFFF, center & 0xFFFFFF);
	XFlush(d);
	sleep(linger);
	XCloseDisplay(d);
	printf("xtest: done\n");
	return 0;
}
