// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul odds and ends the GPU stack links against: HDMI CEC notifiers, the boot
 * framebuffer description, power supplies, perf PMUs, taint and emergency power paths.
 *
 * Each is an interface to a Linux subsystem Nova does not have (the CEC core, sysfb,
 * the power-supply class, perf events, reboot notifiers). The functions report what a
 * Linux kernel built without that subsystem would, so the drivers take their "feature
 * absent" paths.
 */
#include <linux/kernel.h>
#include <linux/panic.h>
#include <linux/printk.h>
#include <linux/reboot.h>
#include <linux/highuid.h>
#include <linux/power_supply.h>
#include <linux/perf_event.h>
#include <linux/sysfb.h>
#include <linux/string.h>
#include <media/cec.h>
#include <media/cec-notifier.h>
#include <asm/video.h>
#include "nova.h"

/* ---------------------------------------------------------------- HDMI CEC */

/* No CEC adapters: notifiers are placeholders (non-NULL, as callers expect on success),
 * like Linux's own stubs without CONFIG_CEC_NOTIFIER. */
struct cec_notifier *cec_notifier_conn_register(struct device *hdmi_dev, const char *port_name,
						const struct cec_connector_info *conn_info)
{
	return (struct cec_notifier *)0xdeadfeed;
}

void cec_notifier_conn_unregister(struct cec_notifier *n)
{
}

void cec_notifier_set_phys_addr(struct cec_notifier *n, u16 pa)
{
}

void cec_fill_conn_info_from_drm(struct cec_connector_info *conn_info, const struct drm_connector *connector)
{
	memset(conn_info, 0, sizeof(*conn_info));
	conn_info->type = CEC_CONNECTOR_TYPE_DRM;
}

/* ---------------------------------------------------------------- boot display */

/* The firmware framebuffer Nova draws on comes from Limine, not from Linux's screen_info;
 * an empty description makes Linux's sysfb code ignore it. */
struct sysfb_display_info sysfb_primary_display;

bool video_is_primary_device(struct device *dev)
{
	return false;
}

/* ---------------------------------------------------------------- power, perf, panic */

/* Without power-supply drivers, Linux reports "on AC power" for a desktop. */
int power_supply_is_system_supplied(void)
{
	return -ENOSYS;
}

/* GPU performance counters are offered to perf, which Nova does not have. */
int perf_pmu_register(struct pmu *pmu, const char *name, int type)
{
	return 0;
}

int perf_pmu_unregister(struct pmu *pmu)
{
	return 0;
}

void perf_event_update_userpage(struct perf_event *event)
{
}

int oops_in_progress;
int overflowuid = DEFAULT_OVERFLOWUID;

void add_taint(unsigned flag, enum lockdep_ok lockdep_ok)
{
	pr_warn("lx_emul: kernel tainted (flag %u)\n", flag);
}

/* amdgpu asks for these when the GPU overheats or fails beyond recovery. Nova keeps
 * running and says so: powering off from a driver is a decision for later. */
void emergency_restart(void)
{
	pr_emerg("lx_emul: a driver requested an emergency restart; ignored\n");
}

void orderly_poweroff(bool force)
{
	pr_emerg("lx_emul: a driver requested a power-off (overheating?); ignored\n");
}
