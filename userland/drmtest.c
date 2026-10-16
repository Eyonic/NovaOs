// drmtest: the Linux GPU user interface (DRM) on Nova, through raw ioctls as libdrm
// issues them. Needs Linux's software DRM drivers: vkms (a virtual display, card0) and
// vgem (GEM buffers and fences), started with nova.vkms=1 nova.vgem=1.
//
// Checks: device nodes and sysfs as libdrm finds them, KMS enumeration, dumb buffers
// drawn through mmap, an atomic modeset and page flips with events read from the card's
// file (poll + read), vblank waits, PRIME export/import between vkms and vgem (dma-buf
// mmap and size), vgem fences seen through sync files and dma-buf poll, and descriptors
// and mappings shared with a child process (fork, SCM_RIGHTS).
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>
#include <drm/vgem_drm.h>
#include <linux/dma-buf.h>
#include <linux/sync_file.h>

static int passed, failed;

static void check(int ok, const char *what, const char *detail)
{
	printf("  %s %s%s%s\n", ok ? "ok  " : "FAIL", what, detail && *detail ? ": " : "", detail ? detail : "");
	if (ok)
		passed++;
	else
		failed++;
}

static int xioctl(int fd, unsigned long req, void *arg)
{
	int r;

	do
		r = ioctl(fd, req, arg);
	while (r == -1 && (errno == EINTR || errno == EAGAIN));
	return r;
}

static double now_ms(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static int driver_name(int fd, char *name, size_t len)
{
	struct drm_version v = { .name = name, .name_len = len - 1 };

	memset(name, 0, len);
	return xioctl(fd, DRM_IOCTL_VERSION, &v);
}

/* The node of driver `want` under /dev/dri whose name starts with `prefix`. */
static int open_driver(const char *want, const char *prefix, char *path, size_t len)
{
	DIR *d = opendir("/dev/dri");
	struct dirent *e;
	int fd = -1;

	if (!d)
		return -1;
	while ((e = readdir(d))) {
		char name[64];

		if (strncmp(e->d_name, prefix, strlen(prefix)))
			continue;
		snprintf(path, len, "/dev/dri/%s", e->d_name);
		fd = open(path, O_RDWR | O_CLOEXEC);
		if (fd < 0)
			continue;
		if (driver_name(fd, name, sizeof(name)) == 0 && !strcmp(name, want))
			break;
		close(fd);
		fd = -1;
	}
	closedir(d);
	return fd;
}

static int read_file(const char *path, char *buf, size_t len)
{
	int fd = open(path, O_RDONLY), n;

	if (fd < 0)
		return -1;
	n = read(fd, buf, len - 1);
	close(fd);
	if (n < 0)
		return -1;
	buf[n] = 0;
	return n;
}

/* sysfs as libdrm's drmGetDevice2 reads it for the node `fd` is open on. */
static void check_sysfs(int fd, const char *bus)
{
	char path[256], buf[256], link[256], real[512], detail[300];
	struct stat st;
	int n;

	if (fstat(fd, &st) || !S_ISCHR(st.st_mode)) {
		check(0, "fstat: a character device", "");
		return;
	}
	snprintf(detail, sizeof(detail), "%u:%u", major(st.st_rdev), minor(st.st_rdev));
	check(major(st.st_rdev) == 226, "fstat: DRM major 226", detail);
	snprintf(path, sizeof(path), "/sys/dev/char/%u:%u/device/drm", major(st.st_rdev), minor(st.st_rdev));
	check(stat(path, &st) == 0 && S_ISDIR(st.st_mode), "sysfs: /sys/dev/char/M:m/device/drm", "");
	fstat(fd, &st);
	snprintf(path, sizeof(path), "/sys/dev/char/%u:%u/device/subsystem", major(st.st_rdev), minor(st.st_rdev));
	n = readlink(path, link, sizeof(link) - 1);
	link[n > 0 ? n : 0] = 0;
	check(n > 0 && strrchr(link, '/') && !strcmp(strrchr(link, '/') + 1, bus), "sysfs: device/subsystem", link);
	snprintf(path, sizeof(path), "/sys/dev/char/%u:%u/device", major(st.st_rdev), minor(st.st_rdev));
	check(realpath(path, real) != NULL, "sysfs: realpath(device)", real);
	snprintf(path, sizeof(path), "/sys/dev/char/%u:%u/uevent", major(st.st_rdev), minor(st.st_rdev));
	check(read_file(path, buf, sizeof(buf)) > 0 && strstr(buf, "DEVNAME=dri/"), "sysfs: uevent DEVNAME", "");
}

/* ---------------------------------------------------------------- KMS */

struct kms {
	uint32_t crtc, conn, plane, mode_blob;
	struct drm_mode_modeinfo mode;
	uint32_t p_crtc_id, p_fb_id, p_src_w, p_src_h, p_crtc_w, p_crtc_h, p_src_x, p_src_y, p_crtc_x, p_crtc_y;
	uint32_t p_conn_crtc, p_mode_id, p_active;
};

static uint32_t prop_id(int fd, uint32_t obj, uint32_t type, const char *name)
{
	struct drm_mode_obj_get_properties op = { .obj_id = obj, .obj_type = type };
	uint32_t ids[64];
	uint64_t vals[64];

	if (xioctl(fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &op) || op.count_props > 64)
		return 0;
	op.props_ptr = (uintptr_t)ids;
	op.prop_values_ptr = (uintptr_t)vals;
	if (xioctl(fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &op))
		return 0;
	for (uint32_t i = 0; i < op.count_props; i++) {
		struct drm_mode_get_property p = { .prop_id = ids[i] };

		if (xioctl(fd, DRM_IOCTL_MODE_GETPROPERTY, &p) == 0 && !strcmp(p.name, name))
			return ids[i];
	}
	return 0;
}

static int kms_setup(int fd, struct kms *k)
{
	struct drm_mode_card_res res = { 0 };
	uint32_t crtcs[8], conns[8], encs[8], fbs[8];
	struct drm_mode_get_connector c = { 0 };
	struct drm_mode_modeinfo modes[64];
	struct drm_mode_get_plane_res pr = { 0 };
	uint32_t planes[16];
	char detail[128];

	check(xioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) == 0 && res.count_crtcs && res.count_connectors,
	      "MODE_GETRESOURCES", "");
	if (res.count_crtcs > 8 || res.count_connectors > 8 || res.count_encoders > 8 || res.count_fbs > 8)
		return -1;
	res.crtc_id_ptr = (uintptr_t)crtcs;
	res.connector_id_ptr = (uintptr_t)conns;
	res.encoder_id_ptr = (uintptr_t)encs;
	res.fb_id_ptr = (uintptr_t)fbs;
	if (xioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res))
		return -1;
	snprintf(detail, sizeof(detail), "%u CRTCs, %u connectors, %u encoders, max %ux%u", res.count_crtcs,
		 res.count_connectors, res.count_encoders, res.max_width, res.max_height);
	check(1, "KMS resources", detail);
	k->crtc = crtcs[0];
	k->conn = conns[0];
	c.connector_id = k->conn;
	if (xioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &c) || !c.count_modes || c.count_modes > 64)
		return -1;
	c.modes_ptr = (uintptr_t)modes;
	c.count_props = c.count_encoders = 0;
	if (xioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &c))
		return -1;
	k->mode = modes[0];
	for (uint32_t i = 0; i < c.count_modes; i++)
		if (modes[i].type & DRM_MODE_TYPE_PREFERRED)
			k->mode = modes[i];
	snprintf(detail, sizeof(detail), "%s, connected %d, %u modes, using %s@%u", "Virtual", c.connection == 1,
		 c.count_modes, k->mode.name, k->mode.vrefresh);
	check(c.connection == 1, "MODE_GETCONNECTOR", detail);

	check(xioctl(fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pr) == 0 && pr.count_planes, "MODE_GETPLANERESOURCES", "");
	if (pr.count_planes > 16)
		return -1;
	pr.plane_id_ptr = (uintptr_t)planes;
	xioctl(fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pr);
	k->plane = 0;
	for (uint32_t i = 0; i < pr.count_planes && !k->plane; i++) {
		uint32_t type_prop = prop_id(fd, planes[i], DRM_MODE_OBJECT_PLANE, "type");
		struct drm_mode_obj_get_properties op = { .obj_id = planes[i], .obj_type = DRM_MODE_OBJECT_PLANE };
		uint32_t ids[64];
		uint64_t vals[64];

		xioctl(fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &op);
		op.props_ptr = (uintptr_t)ids;
		op.prop_values_ptr = (uintptr_t)vals;
		xioctl(fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &op);
		for (uint32_t j = 0; j < op.count_props; j++)
			if (ids[j] == type_prop && vals[j] == 1 /* DRM_PLANE_TYPE_PRIMARY */)
				k->plane = planes[i];
	}
	snprintf(detail, sizeof(detail), "%u planes, primary %u", pr.count_planes, k->plane);
	check(k->plane != 0, "primary plane", detail);

	k->p_crtc_id = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "CRTC_ID");
	k->p_fb_id = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "FB_ID");
	k->p_src_x = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "SRC_X");
	k->p_src_y = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "SRC_Y");
	k->p_src_w = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "SRC_W");
	k->p_src_h = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "SRC_H");
	k->p_crtc_x = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "CRTC_X");
	k->p_crtc_y = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "CRTC_Y");
	k->p_crtc_w = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "CRTC_W");
	k->p_crtc_h = prop_id(fd, k->plane, DRM_MODE_OBJECT_PLANE, "CRTC_H");
	k->p_conn_crtc = prop_id(fd, k->conn, DRM_MODE_OBJECT_CONNECTOR, "CRTC_ID");
	k->p_mode_id = prop_id(fd, k->crtc, DRM_MODE_OBJECT_CRTC, "MODE_ID");
	k->p_active = prop_id(fd, k->crtc, DRM_MODE_OBJECT_CRTC, "ACTIVE");
	check(k->p_fb_id && k->p_mode_id && k->p_active && k->p_conn_crtc, "atomic properties (FB_ID, MODE_ID, ACTIVE, CRTC_ID)", "");
	{
		struct drm_mode_create_blob b = { .data = (uintptr_t)&k->mode, .length = sizeof(k->mode) };

		check(xioctl(fd, DRM_IOCTL_MODE_CREATEPROPBLOB, &b) == 0, "MODE_CREATEPROPBLOB (mode)", "");
		k->mode_blob = b.blob_id;
	}
	return 0;
}

struct dumb {
	uint32_t handle, pitch, fb;
	uint64_t size;
	uint32_t *map;
};

static int dumb_create(int fd, uint32_t w, uint32_t h, struct dumb *b)
{
	struct drm_mode_create_dumb c = { .width = w, .height = h, .bpp = 32 };
	struct drm_mode_map_dumb m = { 0 };

	if (xioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &c))
		return -1;
	b->handle = c.handle;
	b->pitch = c.pitch;
	b->size = c.size;
	m.handle = c.handle;
	if (xioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &m))
		return -1;
	b->map = mmap(NULL, c.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, m.offset);
	return b->map == MAP_FAILED ? -1 : 0;
}

static int add_fb(int fd, struct dumb *b, uint32_t w, uint32_t h)
{
	struct drm_mode_fb_cmd2 f = { .width = w, .height = h, .pixel_format = 0x34325258 /* XR24 */ };

	f.handles[0] = b->handle;
	f.pitches[0] = b->pitch;
	if (xioctl(fd, DRM_IOCTL_MODE_ADDFB2, &f))
		return -1;
	b->fb = f.fb_id;
	return 0;
}

static void draw(struct dumb *b, uint32_t w, uint32_t h, uint32_t seed)
{
	for (uint32_t y = 0; y < h; y++)
		for (uint32_t x = 0; x < w; x++)
			b->map[y * (b->pitch / 4) + x] = (x * 255 / w) << 16 | (y * 255 / h) << 8 | (seed & 0xFF);
}

static int atomic_commit(int fd, uint32_t *objs, uint32_t *counts, uint32_t nobj, uint32_t *props, uint64_t *vals,
			 uint32_t flags)
{
	struct drm_mode_atomic a = {
		.flags = flags,
		.count_objs = nobj,
		.objs_ptr = (uintptr_t)objs,
		.count_props_ptr = (uintptr_t)counts,
		.props_ptr = (uintptr_t)props,
		.prop_values_ptr = (uintptr_t)vals,
		.user_data = 0x1234,
	};

	return xioctl(fd, DRM_IOCTL_MODE_ATOMIC, &a);
}

/* Wait for one DRM event on `fd` (poll, then read); its type or -1. */
static int read_event(int fd, int timeout_ms, uint64_t *user_data, uint32_t *seq)
{
	struct pollfd p = { .fd = fd, .events = POLLIN };
	char buf[256];
	struct drm_event *e = (void *)buf;
	int n;

	if (poll(&p, 1, timeout_ms) != 1 || !(p.revents & POLLIN))
		return -1;
	n = read(fd, buf, sizeof(buf));
	if (n < (int)sizeof(*e))
		return -1;
	if (e->type == DRM_EVENT_FLIP_COMPLETE || e->type == DRM_EVENT_VBLANK) {
		struct drm_event_vblank *v = (void *)buf;

		if (user_data)
			*user_data = v->user_data;
		if (seq)
			*seq = v->sequence;
	}
	return e->type;
}

static void test_kms(int card, int w, int h, struct kms *k, struct dumb bufs[2])
{
	char detail[160];
	uint64_t ud = 0;
	uint32_t seq0 = 0, seq1 = 0;

	check(dumb_create(card, w, h, &bufs[0]) == 0 && dumb_create(card, w, h, &bufs[1]) == 0,
	      "MODE_CREATE_DUMB + MAP_DUMB + mmap (two buffers)", "");
	draw(&bufs[0], w, h, 0x40);
	draw(&bufs[1], w, h, 0xC0);
	check(bufs[0].map[(h / 2) * (bufs[0].pitch / 4) + w / 2] == ((uint32_t)(w / 2 * 255 / w) << 16 | (h / 2 * 255 / h) << 8 | 0x40),
	      "dumb buffer reads back what was drawn", "");
	check(add_fb(card, &bufs[0], w, h) == 0 && add_fb(card, &bufs[1], w, h) == 0, "MODE_ADDFB2 (XR24)", "");

	/* The modeset: connector -> CRTC, CRTC mode + active, plane -> CRTC with fb 0. */
	{
		uint32_t objs[] = { k->conn, k->crtc, k->plane };
		uint32_t counts[] = { 1, 2, 10 };
		uint32_t props[] = { k->p_conn_crtc, k->p_mode_id, k->p_active, k->p_fb_id, k->p_crtc_id, k->p_src_x,
				     k->p_src_y, k->p_src_w, k->p_src_h, k->p_crtc_x, k->p_crtc_y, k->p_crtc_w, k->p_crtc_h };
		uint64_t vals[] = { k->crtc, k->mode_blob, 1, bufs[0].fb, k->crtc, 0, 0, (uint64_t)w << 16, (uint64_t)h << 16, 0, 0, w, h };
		int r = atomic_commit(card, objs, counts, 3, props, vals,
				      DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK);

		snprintf(detail, sizeof(detail), "%s", r ? strerror(errno) : "");
		check(r == 0, "MODE_ATOMIC modeset (nonblocking, with event)", detail);
		check(read_event(card, 2000, &ud, &seq0) == DRM_EVENT_FLIP_COMPLETE && ud == 0x1234,
		      "modeset completion event (poll + read)", "");
	}
	/* Page flips: each completes at a vblank. */
	{
		double t0 = now_ms(), t1;
		int flips = 0;

		for (int i = 0; i < 10; i++) {
			uint32_t objs[] = { k->plane }, counts[] = { 1 }, props[] = { k->p_fb_id };
			uint64_t vals[] = { bufs[(i + 1) & 1].fb };

			if (atomic_commit(card, objs, counts, 1, props, vals, DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK))
				break;
			if (read_event(card, 1000, NULL, &seq1) != DRM_EVENT_FLIP_COMPLETE)
				break;
			flips++;
		}
		t1 = now_ms();
		snprintf(detail, sizeof(detail), "%d flips in %.0f ms (%.1f ms each), vblank %u -> %u", flips, t1 - t0,
			 flips ? (t1 - t0) / flips : 0, seq0, seq1);
		check(flips == 10 && (t1 - t0) / 10 > 8 && seq1 > seq0, "atomic page flips paced by vblank", detail);
	}
	/* The legacy page-flip ioctl too. */
	{
		struct drm_mode_crtc_page_flip pf = { .crtc_id = k->crtc, .fb_id = bufs[0].fb, .flags = DRM_MODE_PAGE_FLIP_EVENT, .user_data = 77 };

		check(xioctl(card, DRM_IOCTL_MODE_PAGE_FLIP, &pf) == 0 && read_event(card, 1000, &ud, NULL) == DRM_EVENT_FLIP_COMPLETE && ud == 77,
		      "MODE_PAGE_FLIP with event", "");
	}
	/* vblank waits: relative, and an event for a future vblank. */
	{
		union drm_wait_vblank v = { .request = { .type = _DRM_VBLANK_RELATIVE, .sequence = 1 } };
		union drm_wait_vblank e = { .request = { .type = _DRM_VBLANK_RELATIVE | _DRM_VBLANK_EVENT, .sequence = 2, .signal = 99 } };
		double t0 = now_ms();
		int r = xioctl(card, DRM_IOCTL_WAIT_VBLANK, &v);

		snprintf(detail, sizeof(detail), "sequence %u, %.1f ms", v.reply.sequence, now_ms() - t0);
		check(r == 0 && v.reply.sequence > 0, "WAIT_VBLANK (blocking)", detail);
		check(xioctl(card, DRM_IOCTL_WAIT_VBLANK, &e) == 0 && read_event(card, 1000, &ud, NULL) == DRM_EVENT_VBLANK && ud == 99,
		      "WAIT_VBLANK with event", "");
	}
	{
		struct drm_mode_crtc c = { .crtc_id = k->crtc };
		int r = xioctl(card, DRM_IOCTL_MODE_GETCRTC, &c);

		snprintf(detail, sizeof(detail), "mode %s, fb %u", c.mode.name, c.fb_id);
		check(r == 0 && c.mode_valid && c.fb_id, "MODE_GETCRTC after modeset", detail);
	}
}

/* ---------------------------------------------------------------- PRIME and fences */

static int prime_export(int fd, uint32_t handle, int *out)
{
	struct drm_prime_handle p = { .handle = handle, .flags = DRM_CLOEXEC | DRM_RDWR };

	if (xioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &p))
		return -1;
	*out = p.fd;
	return 0;
}

static int prime_import(int fd, int dmabuf, uint32_t *handle)
{
	struct drm_prime_handle p = { .fd = dmabuf };

	if (xioctl(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &p))
		return -1;
	*handle = p.handle;
	return 0;
}

static void test_prime(int card, int vgem, int w, int h, struct dumb *b, struct kms *k)
{
	char detail[160];
	int dmabuf = -1, back = -1;
	uint32_t vh = 0;
	off_t size;
	uint32_t *m;

	check(prime_export(card, b->handle, &dmabuf) == 0, "PRIME_HANDLE_TO_FD (vkms buffer -> dma-buf)", "");
	size = lseek(dmabuf, 0, SEEK_END);
	lseek(dmabuf, 0, SEEK_SET);
	snprintf(detail, sizeof(detail), "%lld bytes", (long long)size);
	check(size == (off_t)b->size, "lseek(dma-buf, SEEK_END) gives its size", detail);
	m = mmap(NULL, b->size, PROT_READ | PROT_WRITE, MAP_SHARED, dmabuf, 0);
	check(m != MAP_FAILED && m[5] == b->map[5] && m[b->size / 4 - 1] == b->map[b->size / 4 - 1],
	      "mmap of the dma-buf shows the same pixels", "");
	if (m != MAP_FAILED) {
		struct dma_buf_sync s = { .flags = DMA_BUF_SYNC_START | DMA_BUF_SYNC_RW };

		check(xioctl(dmabuf, DMA_BUF_IOCTL_SYNC, &s) == 0, "DMA_BUF_IOCTL_SYNC", "");
		m[7] = 0xDEADBEEF;
		check(b->map[7] == 0xDEADBEEF, "a write through the dma-buf reaches the buffer", "");
		s.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_RW;
		xioctl(dmabuf, DMA_BUF_IOCTL_SYNC, &s);
		munmap(m, b->size);
	}
	check(vgem >= 0 && prime_import(vgem, dmabuf, &vh) == 0, "PRIME_FD_TO_HANDLE (dma-buf -> vgem)", "");
	/* Exporting the imported buffer again gives back the same dma-buf (one file). */
	{
		struct stat a, b2;
		int again = -1;

		check(vgem >= 0 && prime_export(vgem, vh, &again) == 0 && fstat(again, &b2) == 0 && fstat(dmabuf, &a) == 0 &&
			      a.st_ino == b2.st_ino,
		      "re-export from vgem returns the same dma-buf", "");
		if (again >= 0)
			close(again);
	}
	/* The other way: a vgem buffer shown by vkms. */
	{
		struct drm_mode_create_dumb c = { .width = w, .height = h, .bpp = 32 };
		struct drm_mode_map_dumb md = { 0 };
		uint32_t kh = 0, *vm = MAP_FAILED;
		struct dumb kb = { 0 };
		int r = -1;

		if (vgem >= 0 && xioctl(vgem, DRM_IOCTL_MODE_CREATE_DUMB, &c) == 0) {
			md.handle = c.handle;
			if (xioctl(vgem, DRM_IOCTL_MODE_MAP_DUMB, &md) == 0)
				vm = mmap(NULL, c.size, PROT_READ | PROT_WRITE, MAP_SHARED, vgem, md.offset);
			if (vm != MAP_FAILED) {
				memset(vm, 0x55, c.size);
				munmap(vm, c.size);
			}
			if (prime_export(vgem, c.handle, &back) == 0 && prime_import(card, back, &kh) == 0) {
				kb.handle = kh;
				kb.pitch = c.pitch;
				if (add_fb(card, &kb, w, h) == 0) {
					uint32_t objs[] = { k->plane }, counts[] = { 1 }, props[] = { k->p_fb_id };
					uint64_t vals[] = { kb.fb };

					r = atomic_commit(card, objs, counts, 1, props, vals, DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK);
					if (r == 0)
						r = read_event(card, 1000, NULL, NULL) == DRM_EVENT_FLIP_COMPLETE ? 0 : -1;
				}
			}
		}
		check(r == 0, "vgem buffer -> dma-buf -> vkms framebuffer, flipped onto the screen", "");
	}
	if (back >= 0)
		close(back);
	if (dmabuf >= 0)
		close(dmabuf);
}

static void test_fences(int vgem)
{
	struct drm_mode_create_dumb c = { .width = 64, .height = 64, .bpp = 32 };
	struct drm_vgem_fence_attach fa = { 0 };
	struct drm_vgem_fence_signal fs = { 0 };
	struct dma_buf_export_sync_file es = { .flags = DMA_BUF_SYNC_READ };
	struct sync_file_info info = { 0 };
	int dmabuf = -1;
	char detail[96];

	if (vgem < 0 || xioctl(vgem, DRM_IOCTL_MODE_CREATE_DUMB, &c)) {
		check(0, "vgem buffer for fences", "");
		return;
	}
	fa.handle = c.handle;
	fa.flags = VGEM_FENCE_WRITE;
	check(xioctl(vgem, DRM_IOCTL_VGEM_FENCE_ATTACH, &fa) == 0, "VGEM_FENCE_ATTACH (write fence)", "");
	check(prime_export(vgem, c.handle, &dmabuf) == 0, "export the fenced buffer", "");
	es.fd = -1;
	check(xioctl(dmabuf, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &es) == 0 && es.fd >= 0, "DMA_BUF_IOCTL_EXPORT_SYNC_FILE", "");
	{
		struct pollfd p[2] = { { .fd = es.fd, .events = POLLIN }, { .fd = dmabuf, .events = POLLIN } };

		check(poll(p, 2, 50) == 0, "sync file and dma-buf not ready while the fence is pending", "");
	}
	if (xioctl(es.fd, SYNC_IOC_FILE_INFO, &info) == 0) {
		snprintf(detail, sizeof(detail), "\"%s\", status %d, %u fence(s)", info.name, info.status, info.num_fences);
		check(info.status == 0, "SYNC_IOC_FILE_INFO: active", detail);
	} else {
		check(0, "SYNC_IOC_FILE_INFO", strerror(errno));
	}
	/* Signal from a child process after a delay; the parent sleeps in poll. */
	{
		pid_t pid = fork();
		double t0 = now_ms();
		struct pollfd p = { .fd = es.fd, .events = POLLIN };
		int r, status;

		if (pid == 0) {
			usleep(100 * 1000);
			fs.fence = fa.out_fence;
			_exit(xioctl(vgem, DRM_IOCTL_VGEM_FENCE_SIGNAL, &fs) ? 1 : 0);
		}
		r = poll(&p, 1, 3000);
		snprintf(detail, sizeof(detail), "woke after %.0f ms", now_ms() - t0);
		check(r == 1 && (p.revents & POLLIN), "poll(sync file) wakes when another process signals the fence", detail);
		waitpid(pid, &status, 0);
		check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "VGEM_FENCE_SIGNAL (from the child, on the inherited fd)", "");
	}
	{
		struct pollfd p = { .fd = dmabuf, .events = POLLIN };

		check(poll(&p, 1, 1000) == 1, "poll(dma-buf) ready once its fence signalled", "");
	}
	memset(&info, 0, sizeof(info));
	check(xioctl(es.fd, SYNC_IOC_FILE_INFO, &info) == 0 && info.status == 1, "SYNC_IOC_FILE_INFO: signalled", "");
	close(es.fd);
	close(dmabuf);
}

/* A dma-buf passed to another process over a Unix socket (SCM_RIGHTS). */
static void test_pass(int card, struct dumb *b)
{
	int sv[2], dmabuf;
	pid_t pid;
	int status;

	if (prime_export(card, b->handle, &dmabuf) || socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
		check(0, "SCM_RIGHTS setup", "");
		return;
	}
	pid = fork();
	if (pid == 0) {
		char byte, cbuf[CMSG_SPACE(sizeof(int))];
		struct iovec iov = { .iov_base = &byte, .iov_len = 1 };
		struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof(cbuf) };
		uint32_t *m;
		int fd;

		close(sv[0]);
		if (recvmsg(sv[1], &msg, 0) != 1 || !CMSG_FIRSTHDR(&msg))
			_exit(2);
		memcpy(&fd, CMSG_DATA(CMSG_FIRSTHDR(&msg)), sizeof(int));
		m = mmap(NULL, b->size, PROT_READ, MAP_SHARED, fd, 0);
		if (m == MAP_FAILED)
			_exit(3);
		/* The parent's mapping, inherited by fork, too. */
		_exit(m[7] == 0xDEADBEEF && b->map[7] == 0xDEADBEEF ? 0 : 4);
	}
	{
		char byte = 'x', cbuf[CMSG_SPACE(sizeof(int))];
		struct iovec iov = { .iov_base = &byte, .iov_len = 1 };
		struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof(cbuf) };
		struct cmsghdr *c = CMSG_FIRSTHDR(&msg);

		c->cmsg_level = SOL_SOCKET;
		c->cmsg_type = SCM_RIGHTS;
		c->cmsg_len = CMSG_LEN(sizeof(int));
		memcpy(CMSG_DATA(c), &dmabuf, sizeof(int));
		sendmsg(sv[0], &msg, 0);
	}
	close(dmabuf);
	waitpid(pid, &status, 0);
	check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "dma-buf over SCM_RIGHTS: the child maps it; the forked mapping reads too", "");
	close(sv[0]);
	close(sv[1]);
}

int main(void)
{
	char card_path[300] = "", vgem_path[300] = "", name[64], detail[160];
	struct drm_get_cap cap = { .capability = DRM_CAP_DUMB_BUFFER };
	struct drm_set_client_cap cc = { .capability = DRM_CLIENT_CAP_ATOMIC, .value = 1 };
	struct dumb bufs[2] = { 0 };
	struct kms k = { 0 };
	int card, vgem, w, h;

	printf("drmtest: the Linux GPU interface (DRM) through vkms and vgem\n");
	card = open_driver("vkms", "card", card_path, sizeof(card_path));
	vgem = open_driver("vgem", "card", vgem_path, sizeof(vgem_path));
	if (vgem < 0)
		vgem = open_driver("vgem", "renderD", vgem_path, sizeof(vgem_path));
	snprintf(detail, sizeof(detail), "vkms %s, vgem %s", card_path, vgem_path);
	check(card >= 0 && vgem >= 0, "open /dev/dri nodes", detail);
	if (card < 0) {
		printf("drmtest: %d passed, %d failed\n", passed, failed + 1);
		return 1;
	}
	driver_name(card, name, sizeof(name));
	check(!strcmp(name, "vkms"), "DRM_IOCTL_VERSION", name);
	check_sysfs(card, "faux");
	{
		int r = open_driver("vgem", "renderD", vgem_path, sizeof(vgem_path));

		check(r >= 0, "vgem render node", vgem_path);
		if (r >= 0)
			close(r);
	}
	check(xioctl(card, DRM_IOCTL_GET_CAP, &cap) == 0 && cap.value == 1, "GET_CAP(DUMB_BUFFER)", "");
	cap.capability = DRM_CAP_PRIME;
	check(xioctl(card, DRM_IOCTL_GET_CAP, &cap) == 0 && (cap.value & 3) == 3, "GET_CAP(PRIME): import + export", "");
	check(xioctl(card, DRM_IOCTL_SET_CLIENT_CAP, &cc) == 0, "SET_CLIENT_CAP(ATOMIC)", "");
	if (kms_setup(card, &k) == 0) {
		w = k.mode.hdisplay;
		h = k.mode.vdisplay;
		test_kms(card, w, h, &k, bufs);
		test_prime(card, vgem, w, h, &bufs[0], &k);
		test_pass(card, &bufs[0]);
	}
	test_fences(vgem);
	{
		struct drm_mode_destroy_dumb d = { .handle = bufs[1].handle };

		munmap(bufs[1].map, bufs[1].size);
		check(xioctl(card, DRM_IOCTL_MODE_DESTROY_DUMB, &d) == 0, "munmap + MODE_DESTROY_DUMB", "");
	}
	close(vgem);
	close(card);
	/* A new client after the last one closed: the device came back to a clean state. */
	card = open(card_path, O_RDWR);
	check(card >= 0 && driver_name(card, name, sizeof(name)) == 0, "reopen after close", "");
	close(card);
	printf("drmtest: %d passed, %d failed\n", passed, failed);
	return failed != 0;
}
