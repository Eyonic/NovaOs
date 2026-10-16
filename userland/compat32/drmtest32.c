// drmtest32: DRM from a 32-bit (i386) program, as Steam's client and 32-bit Mesa drive
// the GPU. Calls whose structures differ between i386 and x86-64 (DRM_IOCTL_VERSION,
// DRM_IOCTL_WAIT_VBLANK, DRM_IOCTL_GET_CLIENT) go through the driver's compat_ioctl;
// GEM offsets above 4 GiB are mapped with mmap2. Needs nova.vkms=1.
#define _GNU_SOURCE
#define _FILE_OFFSET_BITS 64
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

static int passed, failed;

static void check(int ok, const char *what, const char *detail)
{
	printf("  %s %s%s%s\n", ok ? "ok  " : "FAIL", what, detail && *detail ? ": " : "", detail ? detail : "");
	if (ok)
		passed++;
	else
		failed++;
}

static int open_vkms(char *path, size_t len)
{
	DIR *d = opendir("/dev/dri");
	struct dirent *e;
	int fd = -1;

	while (d && (e = readdir(d))) {
		char name[32] = "";
		struct drm_version v = { .name = name, .name_len = sizeof(name) - 1 };

		if (strncmp(e->d_name, "card", 4))
			continue;
		snprintf(path, len, "/dev/dri/%.200s", e->d_name);
		fd = open(path, O_RDWR);
		if (fd >= 0 && ioctl(fd, DRM_IOCTL_VERSION, &v) == 0 && !strcmp(name, "vkms"))
			break;
		if (fd >= 0)
			close(fd);
		fd = -1;
	}
	if (d)
		closedir(d);
	return fd;
}

int main(void)
{
	char path[256], name[32] = "", date[32] = "", desc[64] = "", detail[160];
	struct drm_version v = { .name = name, .name_len = sizeof(name) - 1, .date = date, .date_len = sizeof(date) - 1,
				 .desc = desc, .desc_len = sizeof(desc) - 1 };
	struct drm_mode_card_res res = { 0 };
	uint32_t crtcs[4], conns[4];
	struct drm_mode_get_connector c = { 0 };
	struct drm_mode_modeinfo modes[64];
	struct drm_mode_create_dumb cd = { 0 };
	struct drm_mode_map_dumb md = { 0 };
	struct drm_mode_fb_cmd2 fb = { .pixel_format = 0x34325258 };
	struct drm_mode_crtc sc = { 0 };
	uint32_t *map;
	int fd;

	printf("drmtest32: DRM from a 32-bit program (compat_ioctl)\n");
	fd = open_vkms(path, sizeof(path));
	check(fd >= 0, "open the vkms card", path);
	if (fd < 0)
		goto out;
	check(ioctl(fd, DRM_IOCTL_VERSION, &v) == 0 && !strcmp(name, "vkms") && v.desc_len > 0, "DRM_IOCTL_VERSION (converted)", desc);
	{
		struct drm_client cl = { .idx = 0 };

		check(ioctl(fd, DRM_IOCTL_GET_CLIENT, &cl) == 0, "DRM_IOCTL_GET_CLIENT (converted)", "");
	}
	res.crtc_id_ptr = (uintptr_t)crtcs;
	res.connector_id_ptr = (uintptr_t)conns;
	res.count_crtcs = 4;
	res.count_connectors = 4;
	check(ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) == 0 && res.count_crtcs >= 1, "MODE_GETRESOURCES", "");
	c.connector_id = conns[0];
	c.modes_ptr = (uintptr_t)modes;
	c.count_modes = 64;
	check(ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &c) == 0 && c.count_modes > 0, "MODE_GETCONNECTOR", "");
	for (uint32_t i = 0; i < c.count_modes && i < 64; i++)
		if (modes[i].type & DRM_MODE_TYPE_PREFERRED)
			modes[0] = modes[i];
	cd.width = modes[0].hdisplay;
	cd.height = modes[0].vdisplay;
	cd.bpp = 32;
	check(ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &cd) == 0, "MODE_CREATE_DUMB", "");
	md.handle = cd.handle;
	check(ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &md) == 0, "MODE_MAP_DUMB", "");
	snprintf(detail, sizeof(detail), "offset %#llx", (unsigned long long)md.offset);
	map = mmap64(NULL, cd.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, md.offset);
	check(map != MAP_FAILED, "mmap64 of the dumb buffer (offset beyond 4 GiB, through mmap2)", detail);
	if (map != MAP_FAILED) {
		for (uint32_t i = 0; i < cd.size / 4; i++)
			map[i] = 0x00336699 + i;
		check(map[1234] == 0x00336699 + 1234, "draw and read back", "");
	}
	fb.width = cd.width;
	fb.height = cd.height;
	fb.handles[0] = cd.handle;
	fb.pitches[0] = cd.pitch;
	check(ioctl(fd, DRM_IOCTL_MODE_ADDFB2, &fb) == 0, "MODE_ADDFB2", "");
	sc.crtc_id = crtcs[0];
	sc.fb_id = fb.fb_id;
	sc.set_connectors_ptr = (uintptr_t)conns;
	sc.count_connectors = 1;
	sc.mode = modes[0];
	sc.mode_valid = 1;
	{
		int r = ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &sc);

		snprintf(detail, sizeof(detail), "%s %s", modes[0].name, r ? strerror(errno) : "");
		check(r == 0, "MODE_SETCRTC (legacy modeset)", detail);
	}
	{
		union drm_wait_vblank w = { .request = { .type = _DRM_VBLANK_RELATIVE, .sequence = 1 } };
		int r = ioctl(fd, DRM_IOCTL_WAIT_VBLANK, &w);

		snprintf(detail, sizeof(detail), "sequence %u, at %ld.%06ld", w.reply.sequence, (long)w.reply.tval_sec, (long)w.reply.tval_usec);
		check(r == 0 && w.reply.tval_sec > 0, "WAIT_VBLANK (converted: 32-bit timeval)", detail);
	}
	{
		struct drm_prime_handle p = { .handle = cd.handle, .flags = DRM_CLOEXEC };

		check(ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &p) == 0 && lseek(p.fd, 0, SEEK_END) == (off_t)cd.size,
		      "PRIME export and dma-buf size", "");
		close(p.fd);
	}
	close(fd);
out:
	printf("drmtest32: %d passed, %d failed\n", passed, failed);
	return failed != 0;
}
