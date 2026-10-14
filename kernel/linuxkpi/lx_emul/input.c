// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul input bridge: Linux input devices (USB keyboards, mice, tablets; later gamepads)
 * feed Nova's own input handling.
 *
 * Linux's input core (drivers/input/input.c, real) connects every input device to every
 * matching input_handler. Nova registers one handler that matches all devices and passes
 * key, relative, absolute and sync events to nova_input_event (linuxkpi.rs), which turns
 * keys into PS/2 set 1 scan codes and pointer motion into mouse reports. Absolute axes are
 * scaled here to 0..0xFFFF, the range Nova's absolute pointer uses.
 */
#include <linux/input.h>
#include <linux/slab.h>
#include <linux/init.h>
#include "nova.h"

void nova_input_event(u16 type, u16 code, s32 value);

static void nova_input_handle_event(struct input_handle *handle, unsigned int type, unsigned int code, int value)
{
	if (type == EV_ABS && (code == ABS_X || code == ABS_Y) && handle->dev->absinfo) {
		const struct input_absinfo *abs = &handle->dev->absinfo[code];
		long range = (long)abs->maximum - abs->minimum;

		value = range > 0 ? (int)(((long)value - abs->minimum) * 0xFFFF / range) : 0;
	}
	if (type == EV_KEY || type == EV_REL || type == EV_ABS || type == EV_SYN)
		nova_input_event(type, code, value);
}

static int nova_input_connect(struct input_handler *handler, struct input_dev *dev, const struct input_device_id *id)
{
	struct input_handle *handle = kzalloc(sizeof(*handle), GFP_KERNEL);
	int err;

	if (!handle)
		return -ENOMEM;
	handle->dev = dev;
	handle->handler = handler;
	handle->name = "nova";
	err = input_register_handle(handle);
	if (!err)
		err = input_open_device(handle);
	if (err) {
		input_unregister_handle(handle);
		kfree(handle);
		return err;
	}
	pr_info("input: %s\n", dev->name ?: "unnamed device");
	return 0;
}

static void nova_input_disconnect(struct input_handle *handle)
{
	input_close_device(handle);
	input_unregister_handle(handle);
	kfree(handle);
}

static const struct input_device_id nova_input_ids[] = {
	/* Every device: an event-bit match with an empty mask (the table ends at flags == 0). */
	{ .flags = INPUT_DEVICE_ID_MATCH_EVBIT },
	{ },
};

static struct input_handler nova_input_handler = {
	.event = nova_input_handle_event,
	.connect = nova_input_connect,
	.disconnect = nova_input_disconnect,
	.name = "nova",
	.id_table = nova_input_ids,
};

static int __init nova_input_init(void)
{
	return input_register_handler(&nova_input_handler);
}
/* After the input core (subsys_initcall), before device drivers. */
fs_initcall(nova_input_init);
