// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul driver core: buses, drivers, devices and device-managed resources, after
 * Linux's drivers/base (which is tied to sysfs/kernfs, so it is reimplemented here).
 *
 * What Linux code relies on is kept: matching (bus->match) and probing (bus->probe or
 * drv->probe) when a device is added to a bus with drivers, or a driver to a bus with
 * devices, in the same order as Linux; unbinding on removal; reference counting through
 * the device's kobject (put_device() calls the release function); devres lists released
 * on unbind. Not kept: sysfs (attribute files are accepted and ignored), uevents, device
 * links, classes beyond a name, asynchronous probing (probes run synchronously).
 *
 * struct device_private and struct driver_private are private to drivers/base in Linux;
 * lx_emul defines its own.
 */
#include <linux/device.h>
#include <linux/device/bus.h>
#include <linux/device/class.h>
#include <linux/device/driver.h>
#include <linux/kobject.h>
#include <linux/slab.h>
#include <linux/list.h>
#include <linux/notifier.h>
#include <linux/cdev.h>
#include <linux/fs.h>
#include <linux/pm_runtime.h>
#include "nova.h"

struct device_private {
	struct list_head bus_node;	/* on the bus's device list */
	struct device *device;
};

struct driver_private {
	struct list_head bus_node;	/* on the bus's driver list */
	struct device_driver *driver;
};

struct lx_bus {
	const struct bus_type *bus;
	struct list_head devices;
	struct list_head drivers;
	struct blocking_notifier_head notifier;
};

static struct lx_bus lx_buses[16];

static struct lx_bus *lx_bus_find(const struct bus_type *bus, bool create)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(lx_buses); i++)
		if (lx_buses[i].bus == bus)
			return &lx_buses[i];
	if (!create)
		return NULL;
	for (i = 0; i < ARRAY_SIZE(lx_buses); i++) {
		if (!lx_buses[i].bus) {
			lx_buses[i].bus = bus;
			INIT_LIST_HEAD(&lx_buses[i].devices);
			INIT_LIST_HEAD(&lx_buses[i].drivers);
			BLOCKING_INIT_NOTIFIER_HEAD(&lx_buses[i].notifier);
			return &lx_buses[i];
		}
	}
	return NULL;
}

/* ---------------------------------------------------------------- kobjects */

static void lx_device_release(struct kref *kref)
{
	struct device *dev = container_of(kref, struct device, kobj.kref);

	kfree(dev->p);
	dev->p = NULL;
	kfree_const(dev->kobj.name);
	dev->kobj.name = NULL;
	if (dev->release)
		dev->release(dev);
	else if (dev->type && dev->type->release)
		dev->type->release(dev);
	else if (dev->class && dev->class->dev_release)
		dev->class->dev_release(dev);
	else
		pr_warn("device %s has no release function; leaked\n", dev_name(dev));
}

struct device *get_device(struct device *dev)
{
	if (dev)
		kref_get(&dev->kobj.kref);
	return dev;
}

void put_device(struct device *dev)
{
	if (dev)
		kref_put(&dev->kobj.kref, lx_device_release);
}

struct kobject *kobject_get(struct kobject *kobj)
{
	if (kobj)
		kref_get(&kobj->kref);
	return kobj;
}

void kobject_put(struct kobject *kobj)
{
	/* Only device kobjects are created by lx_emul code paths. */
	if (kobj)
		put_device(container_of(kobj, struct device, kobj));
}

int kobject_set_name_vargs(struct kobject *kobj, const char *fmt, va_list vargs)
{
	const char *name = kvasprintf_const(GFP_KERNEL, fmt, vargs);

	if (!name)
		return -ENOMEM;
	kfree_const(kobj->name);
	kobj->name = name;
	return 0;
}

int dev_set_name(struct device *dev, const char *fmt, ...)
{
	va_list vargs;
	int err;

	va_start(vargs, fmt);
	err = kobject_set_name_vargs(&dev->kobj, fmt, vargs);
	va_end(vargs);
	return err;
}

char *kobject_get_path(const struct kobject *kobj, gfp_t gfp)
{
	return kstrdup(kobject_name(kobj) ?: "", gfp);
}

/* No sysfs, so no uevents. */
int kobject_uevent(struct kobject *kobj, enum kobject_action action)
{
	return 0;
}

int kobject_uevent_env(struct kobject *kobj, enum kobject_action action, char *envp[])
{
	return 0;
}

int add_uevent_var(struct kobj_uevent_env *env, const char *format, ...)
{
	return 0;
}

int device_create_file(struct device *device, const struct device_attribute *entry)
{
	return 0;
}

void device_remove_file(struct device *dev, const struct device_attribute *attr)
{
}

int driver_create_file(const struct device_driver *driver, const struct driver_attribute *attr)
{
	return 0;
}

void driver_remove_file(const struct device_driver *driver, const struct driver_attribute *attr)
{
}

/* ---------------------------------------------------------------- device-managed resources */

struct lx_devres {
	struct list_head node;
	dr_release_t release;
	const char *name;
	size_t size;
	u8 data[] __aligned(ARCH_KMALLOC_MINALIGN);
};

/* A group marker: release() is NULL and data holds the group id. */
static void lx_devres_group_marker(struct device *dev, void *res)
{
}

void *__devres_alloc_node(dr_release_t release, size_t size, gfp_t gfp, int nid, const char *name)
{
	struct lx_devres *dr = kzalloc(sizeof(*dr) + size, gfp);

	if (!dr)
		return NULL;
	INIT_LIST_HEAD(&dr->node);
	dr->release = release;
	dr->name = name;
	dr->size = size;
	return dr->data;
}

static struct lx_devres *lx_devres(void *res)
{
	return container_of(res, struct lx_devres, data);
}

void devres_add(struct device *dev, void *res)
{
	unsigned long flags;

	spin_lock_irqsave(&dev->devres_lock, flags);
	list_add_tail(&lx_devres(res)->node, &dev->devres_head);
	spin_unlock_irqrestore(&dev->devres_lock, flags);
}

void devres_free(void *res)
{
	if (res)
		kfree(lx_devres(res));
}

static struct lx_devres *lx_devres_find(struct device *dev, dr_release_t release, dr_match_t match,
					void *match_data)
{
	struct lx_devres *dr;

	list_for_each_entry_reverse(dr, &dev->devres_head, node)
		if (dr->release == release && (!match || match(dev, dr->data, match_data)))
			return dr;
	return NULL;
}

void *devres_find(struct device *dev, dr_release_t release, dr_match_t match, void *match_data)
{
	struct lx_devres *dr = lx_devres_find(dev, release, match, match_data);

	return dr ? dr->data : NULL;
}

void *devres_remove(struct device *dev, dr_release_t release, dr_match_t match, void *match_data)
{
	struct lx_devres *dr = lx_devres_find(dev, release, match, match_data);

	if (!dr)
		return NULL;
	list_del_init(&dr->node);
	return dr->data;
}

int devres_destroy(struct device *dev, dr_release_t release, dr_match_t match, void *match_data)
{
	void *res = devres_remove(dev, release, match, match_data);

	if (!res)
		return -ENOENT;
	devres_free(res);
	return 0;
}

int devres_release(struct device *dev, dr_release_t release, dr_match_t match, void *match_data)
{
	void *res = devres_remove(dev, release, match, match_data);

	if (!res)
		return -ENOENT;
	release(dev, res);
	devres_free(res);
	return 0;
}

/* Release (in reverse order) every resource added after `marker` (or all, for NULL). */
static int lx_devres_release_from(struct device *dev, struct lx_devres *marker)
{
	int n = 0;

	while (!list_empty(&dev->devres_head)) {
		struct lx_devres *dr = list_last_entry(&dev->devres_head, struct lx_devres, node);

		list_del_init(&dr->node);
		if (dr == marker) {
			kfree(dr);
			break;
		}
		if (dr->release && dr->release != lx_devres_group_marker)
			dr->release(dev, dr->data);
		kfree(dr);
		n++;
	}
	return n;
}

static int devres_release_all(struct device *dev)
{
	if (!dev->devres_head.next)
		return 0;
	return lx_devres_release_from(dev, NULL);
}

void *devres_open_group(struct device *dev, void *id, gfp_t gfp)
{
	void *res = __devres_alloc_node(lx_devres_group_marker, sizeof(void *), gfp, NUMA_NO_NODE, "group");

	if (!res)
		return NULL;
	*(void **)res = id ?: res;
	devres_add(dev, res);
	return *(void **)res;
}

static struct lx_devres *lx_devres_group(struct device *dev, void *id)
{
	struct lx_devres *dr;

	list_for_each_entry_reverse(dr, &dev->devres_head, node)
		if (dr->release == lx_devres_group_marker && (!id || *(void **)dr->data == id))
			return dr;
	return NULL;
}

void devres_close_group(struct device *dev, void *id)
{
}

void devres_remove_group(struct device *dev, void *id)
{
	struct lx_devres *dr = lx_devres_group(dev, id);

	if (dr) {
		list_del_init(&dr->node);
		kfree(dr);
	}
}

int devres_release_group(struct device *dev, void *id)
{
	struct lx_devres *dr = lx_devres_group(dev, id);

	return dr ? lx_devres_release_from(dev, dr) : -ENOENT;
}

struct lx_action {
	void (*action)(void *);
	void *data;
};

static void lx_action_release(struct device *dev, void *res)
{
	struct lx_action *a = res;

	a->action(a->data);
}

int __devm_add_action(struct device *dev, void (*action)(void *), void *data, const char *name)
{
	struct lx_action *a = __devres_alloc_node(lx_action_release, sizeof(*a), GFP_KERNEL, NUMA_NO_NODE, name);

	if (!a)
		return -ENOMEM;
	a->action = action;
	a->data = data;
	devres_add(dev, a);
	return 0;
}

/* ---------------------------------------------------------------- devices */

void device_initialize(struct device *dev)
{
	kref_init(&dev->kobj.kref);
	INIT_LIST_HEAD(&dev->dma_pools);
	mutex_init(&dev->mutex);
	spin_lock_init(&dev->devres_lock);
	INIT_LIST_HEAD(&dev->devres_head);
	dev->links.status = DL_DEV_NO_DRIVER;
}

static int lx_really_probe(struct device *dev, struct device_driver *drv)
{
	int ret;

	dev->driver = drv;
	if (dev->bus && dev->bus->probe)
		ret = dev->bus->probe(dev);
	else if (drv->probe)
		ret = drv->probe(dev);
	else
		ret = 0;
	if (ret) {
		devres_release_all(dev);
		dev->driver = NULL;
		dev_set_drvdata(dev, NULL);
		if (ret != -ENODEV && ret != -ENXIO)
			dev_warn(dev, "probe of %s failed: %d\n", drv->name, ret);
		return ret;
	}
	dev->links.status = DL_DEV_DRIVER_BOUND;
	return 0;
}

/* 1 if bound, 0 if not, as Linux's driver_probe_device. */
static int lx_try_driver(struct device *dev, struct device_driver *drv)
{
	int match = dev->bus && dev->bus->match ? dev->bus->match(dev, drv) : 1;

	if (match <= 0 || dev->driver)
		return 0;
	return lx_really_probe(dev, drv) == 0;
}

int device_attach(struct device *dev)
{
	struct lx_bus *b = dev->bus ? lx_bus_find(dev->bus, false) : NULL;
	struct driver_private *dp;

	if (dev->driver)
		return 1;
	if (!b)
		return 0;
	list_for_each_entry(dp, &b->drivers, bus_node)
		if (lx_try_driver(dev, dp->driver))
			return 1;
	return 0;
}

void device_initial_probe(struct device *dev)
{
	if (device_attach(dev) < 0)
		dev_dbg(dev, "no driver bound\n");
}

int device_bind_driver(struct device *dev)
{
	dev->links.status = DL_DEV_DRIVER_BOUND;
	return 0;
}

void device_release_driver(struct device *dev)
{
	struct device_driver *drv = dev->driver;

	if (!drv)
		return;
	if (dev->bus && dev->bus->remove)
		dev->bus->remove(dev);
	else if (drv->remove)
		drv->remove(dev);
	devres_release_all(dev);
	dev->driver = NULL;
	dev_set_drvdata(dev, NULL);
	dev->links.status = DL_DEV_NO_DRIVER;
}

int device_reprobe(struct device *dev)
{
	device_release_driver(dev);
	return device_attach(dev) < 0 ? -EINVAL : 0;
}

int device_add(struct device *dev)
{
	struct lx_bus *b;

	if (!dev->kobj.kref.refcount.refs.counter)
		device_initialize(dev);
	if (dev->init_name) {
		dev_set_name(dev, "%s", dev->init_name);
		dev->init_name = NULL;
	}
	if (!dev_name(dev))
		dev_set_name(dev, "%s%u", dev->bus ? dev->bus->name : "dev", dev->id);
	dev->p = kzalloc(sizeof(*dev->p), GFP_KERNEL);
	if (!dev->p)
		return -ENOMEM;
	dev->p->device = dev;
	INIT_LIST_HEAD(&dev->p->bus_node);
	get_device(dev->parent);

	b = dev->bus ? lx_bus_find(dev->bus, true) : NULL;
	if (b) {
		blocking_notifier_call_chain(&b->notifier, BUS_NOTIFY_ADD_DEVICE, dev);
		list_add_tail(&dev->p->bus_node, &b->devices);
		/* As bus_probe_device(): bind a driver now if one matches. */
		if (device_attach(dev) < 0)
			dev_dbg(dev, "no driver bound\n");
	}
	return 0;
}

int device_register(struct device *dev)
{
	device_initialize(dev);
	return device_add(dev);
}

void device_del(struct device *dev)
{
	struct lx_bus *b = dev->bus ? lx_bus_find(dev->bus, false) : NULL;

	if (b)
		blocking_notifier_call_chain(&b->notifier, BUS_NOTIFY_DEL_DEVICE, dev);
	device_release_driver(dev);
	if (dev->p)
		list_del_init(&dev->p->bus_node);
	put_device(dev->parent);
}

void device_unregister(struct device *dev)
{
	device_del(dev);
	put_device(dev);
}

/* ---------------------------------------------------------------- drivers and buses */

int driver_attach(const struct device_driver *drv)
{
	struct lx_bus *b = lx_bus_find(drv->bus, false);
	struct device_private *p, *next;

	if (!b)
		return 0;
	list_for_each_entry_safe(p, next, &b->devices, bus_node)
		lx_try_driver(p->device, (struct device_driver *)drv);
	return 0;
}

int driver_register(struct device_driver *drv)
{
	struct lx_bus *b = lx_bus_find(drv->bus, true);

	if (!b)
		return -ENOMEM;
	drv->p = kzalloc(sizeof(*drv->p), GFP_KERNEL);
	if (!drv->p)
		return -ENOMEM;
	drv->p->driver = drv;
	list_add_tail(&drv->p->bus_node, &b->drivers);
	return driver_attach(drv);
}

void driver_unregister(struct device_driver *drv)
{
	struct lx_bus *b = lx_bus_find(drv->bus, false);
	struct device_private *p;

	if (!b || !drv->p)
		return;
	list_for_each_entry(p, &b->devices, bus_node)
		if (p->device->driver == drv)
			device_release_driver(p->device);
	list_del(&drv->p->bus_node);
	kfree(drv->p);
	drv->p = NULL;
}

int bus_register(const struct bus_type *bus)
{
	return lx_bus_find(bus, true) ? 0 : -ENOMEM;
}

void bus_unregister(const struct bus_type *bus)
{
}

int bus_register_notifier(const struct bus_type *bus, struct notifier_block *nb)
{
	struct lx_bus *b = lx_bus_find(bus, true);

	return b ? blocking_notifier_chain_register(&b->notifier, nb) : -ENOMEM;
}

int bus_unregister_notifier(const struct bus_type *bus, struct notifier_block *nb)
{
	struct lx_bus *b = lx_bus_find(bus, false);

	return b ? blocking_notifier_chain_unregister(&b->notifier, nb) : -ENOENT;
}

int bus_for_each_dev(const struct bus_type *bus, struct device *start, void *data, device_iter_t fn)
{
	struct lx_bus *b = lx_bus_find(bus, false);
	struct device_private *p, *next;
	bool started = !start;
	int err = 0;

	if (!b)
		return 0;
	list_for_each_entry_safe(p, next, &b->devices, bus_node) {
		if (!started) {
			started = p->device == start;
			continue;
		}
		err = fn(p->device, data);
		if (err)
			break;
	}
	return err;
}

struct device *bus_find_device(const struct bus_type *bus, struct device *start, const void *data,
			       device_match_t match)
{
	struct lx_bus *b = lx_bus_find(bus, false);
	struct device_private *p;
	bool started = !start;

	if (!b)
		return NULL;
	list_for_each_entry(p, &b->devices, bus_node) {
		if (!started) {
			started = p->device == start;
			continue;
		}
		if (match(p->device, data))
			return get_device(p->device);
	}
	return NULL;
}

int bus_for_each_drv(const struct bus_type *bus, struct device_driver *start, void *data,
		     int (*fn)(struct device_driver *, void *))
{
	struct lx_bus *b = lx_bus_find(bus, false);
	struct driver_private *dp;
	bool started = !start;
	int err = 0;

	if (!b)
		return 0;
	list_for_each_entry(dp, &b->drivers, bus_node) {
		if (!started) {
			started = dp->driver == start;
			continue;
		}
		err = fn(dp->driver, data);
		if (err)
			break;
	}
	return err;
}

int bus_rescan_devices(const struct bus_type *bus)
{
	struct lx_bus *b = lx_bus_find(bus, false);
	struct device_private *p;

	if (b)
		list_for_each_entry(p, &b->devices, bus_node)
			if (device_attach(p->device) < 0)
				dev_dbg(p->device, "rescan found no driver\n");
	return 0;
}

int device_match_devt(struct device *dev, const void *pdevt)
{
	return dev->devt == *(dev_t *)pdevt;
}

/* ---------------------------------------------------------------- classes and char devices */

int class_register(const struct class *cls)
{
	return 0;
}

void class_unregister(const struct class *cls)
{
}

static void lx_class_device_release(struct device *dev)
{
	kfree(dev);
}

struct device *device_create(const struct class *cls, struct device *parent, dev_t devt, void *drvdata,
			     const char *fmt, ...)
{
	struct device *dev = kzalloc(sizeof(*dev), GFP_KERNEL);
	va_list vargs;

	if (!dev)
		return ERR_PTR(-ENOMEM);
	device_initialize(dev);
	dev->class = cls;
	dev->parent = parent;
	dev->devt = devt;
	dev->release = lx_class_device_release;
	dev_set_drvdata(dev, drvdata);
	va_start(vargs, fmt);
	kobject_set_name_vargs(&dev->kobj, fmt, vargs);
	va_end(vargs);
	if (device_add(dev)) {
		put_device(dev);
		return ERR_PTR(-ENOMEM);
	}
	return dev;
}

void device_destroy(const struct class *cls, dev_t devt)
{
}

/* Character device nodes appear with Nova's device filesystem (later); registering one
 * succeeds so drivers carry on. */
int register_chrdev_region(dev_t from, unsigned count, const char *name)
{
	return 0;
}

void unregister_chrdev_region(dev_t from, unsigned count)
{
}

int __register_chrdev(unsigned int major, unsigned int baseminor, unsigned int count, const char *name,
		      const struct file_operations *fops)
{
	return major ?: 240;
}

void __unregister_chrdev(unsigned int major, unsigned int baseminor, unsigned int count, const char *name)
{
}

void cdev_init(struct cdev *cdev, const struct file_operations *fops)
{
	memset(cdev, 0, sizeof(*cdev));
	cdev->ops = fops;
}

int cdev_add(struct cdev *p, dev_t dev, unsigned count)
{
	p->dev = dev;
	p->count = count;
	return 0;
}

void cdev_del(struct cdev *p)
{
}
