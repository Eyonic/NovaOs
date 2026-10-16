// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul firmware loading and firmware-node properties.
 *
 * request_firmware() and friends read /lib/firmware/<name> (or /usr/lib/firmware) from
 * Nova's file systems through nova_firmware_read(); there is no user-space helper and no
 * fallback. Device properties come from ACPI _DSD or device trees on Linux; Nova's PCI
 * devices have none, so lookups report that the property is absent, as Linux does for a
 * device without a firmware node.
 */
#include <linux/firmware.h>
#include <linux/property.h>
#include <linux/slab.h>
#include <linux/errno.h>
#include "nova.h"

static int lx_firmware_load(const struct firmware **fw, const char *name, bool warn)
{
	struct firmware *f;
	size_t len = 0;
	u8 *data;

	*fw = NULL;
	data = nova_firmware_read(name, strlen(name), &len);
	if (!data) {
		if (warn)
			pr_warn("firmware: %s not found in /lib/firmware\n", name);
		return -ENOENT;
	}
	f = kzalloc(sizeof(*f), GFP_KERNEL);
	if (!f) {
		nova_free(data);
		return -ENOMEM;
	}
	f->data = data;
	f->size = len;
	*fw = f;
	return 0;
}

int request_firmware(const struct firmware **fw, const char *name, struct device *device)
{
	return lx_firmware_load(fw, name, true);
}

int firmware_request_nowarn(const struct firmware **fw, const char *name, struct device *device)
{
	return lx_firmware_load(fw, name, false);
}

int request_firmware_direct(const struct firmware **fw, const char *name, struct device *device)
{
	return lx_firmware_load(fw, name, false);
}

void release_firmware(const struct firmware *fw)
{
	if (!fw)
		return;
	nova_free(fw->data);
	kfree(fw);
}

/* ---------------------------------------------------------------- properties */

struct fwnode_handle *fwnode_handle_get(struct fwnode_handle *fwnode)
{
	return fwnode;
}

struct fwnode_handle *fwnode_find_reference(const struct fwnode_handle *fwnode, const char *name,
					    unsigned int index)
{
	return ERR_PTR(-ENOENT);
}

struct fwnode_handle *fwnode_get_named_child_node(const struct fwnode_handle *fwnode,
						  const char *childname)
{
	return NULL;
}

int fwnode_property_read_string(const struct fwnode_handle *fwnode, const char *propname,
				const char **val)
{
	return -EINVAL;
}

int fwnode_property_read_u32_array(const struct fwnode_handle *fwnode, const char *propname,
				   u32 *val, size_t nval)
{
	return -EINVAL;
}

int device_property_read_u32_array(const struct device *dev, const char *propname, u32 *val,
				   size_t nval)
{
	return -EINVAL;
}
