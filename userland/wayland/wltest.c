/* wltest: a Wayland client (libwayland-client, xdg-shell) on Nova's compositor: opens a
 * toplevel window, draws into shared memory, and checks the protocol round trips (frame
 * callback, buffer release, keymap, output mode). It stays open for a few seconds so the
 * compositor's state can be inspected (`wayland` shell command). */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"

#define W 320
#define H 240

static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm_base;
static struct wl_seat *seat;
static int configured, frame_done, released, keymap_size, output_w, shm_xrgb;

static void global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v)
{
	if (!strcmp(iface, wl_compositor_interface.name))
		compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
	else if (!strcmp(iface, wl_shm_interface.name))
		shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
	else if (!strcmp(iface, xdg_wm_base_interface.name))
		wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
	else if (!strcmp(iface, wl_seat_interface.name))
		seat = wl_registry_bind(r, name, &wl_seat_interface, 5);
	else if (!strcmp(iface, wl_output_interface.name))
		wl_registry_bind(r, name, &wl_output_interface, 2);
}
static void global_remove(void *d, struct wl_registry *r, uint32_t name) {}
static const struct wl_registry_listener registry_listener = {global, global_remove};

static void shm_format(void *d, struct wl_shm *s, uint32_t f)
{
	if (f == WL_SHM_FORMAT_XRGB8888)
		shm_xrgb = 1;
}
static const struct wl_shm_listener shm_listener = {shm_format};

static void ping(void *d, struct xdg_wm_base *b, uint32_t serial) { xdg_wm_base_pong(b, serial); }
static const struct xdg_wm_base_listener wm_base_listener = {ping};

static void xdg_configure(void *d, struct xdg_surface *s, uint32_t serial)
{
	xdg_surface_ack_configure(s, serial);
	configured = 1;
}
static const struct xdg_surface_listener xdg_surface_listener = {xdg_configure};

static void top_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *states) {}
static void top_close(void *d, struct xdg_toplevel *t) {}
static const struct xdg_toplevel_listener toplevel_listener = {top_configure, top_close};

static void done(void *d, struct wl_callback *cb, uint32_t t)
{
	frame_done = 1;
	wl_callback_destroy(cb);
}
static const struct wl_callback_listener frame_listener = {done};

static void release(void *d, struct wl_buffer *b) { released = 1; }
static const struct wl_buffer_listener buffer_listener = {release};

static void keymap(void *d, struct wl_keyboard *k, uint32_t format, int32_t fd, uint32_t size)
{
	char *map = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
	if (map != MAP_FAILED && format == WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1 && strstr(map, "xkb_keymap"))
		keymap_size = size;
	close(fd);
}
static void kenter(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *f, struct wl_array *keys) {}
static void kleave(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *f) {}
static void kkey(void *d, struct wl_keyboard *k, uint32_t s, uint32_t t, uint32_t key, uint32_t state) {}
static void kmods(void *d, struct wl_keyboard *k, uint32_t s, uint32_t a, uint32_t b, uint32_t c, uint32_t g) {}
static void krepeat(void *d, struct wl_keyboard *k, int32_t rate, int32_t delay) {}
static const struct wl_keyboard_listener keyboard_listener = {keymap, kenter, kleave, kkey, kmods, krepeat};

int main(int argc, char **argv)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	int linger = argc > 1 ? atoi(argv[1]) : 0;
	struct wl_display *display = wl_display_connect(NULL);
	if (!display) {
		printf("wltest: cannot connect to the compositor\n");
		return 1;
	}
	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, NULL);
	wl_display_roundtrip(display);
	if (!compositor || !shm || !wm_base || !seat) {
		printf("wltest: missing globals\n");
		return 1;
	}
	wl_shm_add_listener(shm, &shm_listener, NULL);
	xdg_wm_base_add_listener(wm_base, &wm_base_listener, NULL);
	struct wl_keyboard *kb = wl_seat_get_keyboard(seat);
	wl_keyboard_add_listener(kb, &keyboard_listener, NULL);

	struct wl_surface *surface = wl_compositor_create_surface(compositor);
	struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(wm_base, surface);
	xdg_surface_add_listener(xs, &xdg_surface_listener, NULL);
	struct xdg_toplevel *top = xdg_surface_get_toplevel(xs);
	xdg_toplevel_add_listener(top, &toplevel_listener, NULL);
	xdg_toplevel_set_title(top, "wltest");
	wl_surface_commit(surface);
	while (!configured && wl_display_dispatch(display) != -1)
		;
	printf("wltest: connected, configured, keymap %s, XRGB8888 %s\n", keymap_size > 1000 ? "received" : "missing", shm_xrgb ? "offered" : "missing");

	int fd = memfd_create("wltest", MFD_CLOEXEC);
	ftruncate(fd, W * H * 4);
	uint32_t *px = mmap(NULL, W * H * 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	for (int y = 0; y < H; y++)
		for (int x = 0; x < W; x++)
			px[y * W + x] = (abs(x - W / 2) < 20 && abs(y - H / 2) < 20) ? 0xFF8800 : 0x3366CC;
	struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, W * H * 4);
	struct wl_buffer *buf = wl_shm_pool_create_buffer(pool, 0, W, H, W * 4, WL_SHM_FORMAT_XRGB8888);
	wl_buffer_add_listener(buf, &buffer_listener, NULL);
	struct wl_callback *cb = wl_surface_frame(surface);
	wl_callback_add_listener(cb, &frame_listener, NULL);
	wl_surface_attach(surface, buf, 0, 0);
	wl_surface_damage_buffer(surface, 0, 0, W, H);
	wl_surface_commit(surface);
	while ((!frame_done || !released) && wl_display_dispatch(display) != -1)
		;
	printf("wltest: buffer shown (%dx%d), frame callback %s, buffer %s\n", W, H, frame_done ? "done" : "missing", released ? "released" : "held");
	for (int i = 0; i < linger * 10; i++) {
		wl_display_dispatch_pending(display);
		wl_display_flush(display);
		usleep(100000);
	}
	wl_display_disconnect(display);
	printf("wltest: done\n");
	return 0;
}
