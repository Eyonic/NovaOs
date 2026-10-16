// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul support for larger subsystems (USB, input, later DRM): slab caches, RCU
 * callbacks, platform devices, scatter-gather DMA, firmware-node properties, and the
 * parts of the VFS and process model that drivers touch but Nova does not have.
 */
#include <linux/slab.h>
#include <linux/rcupdate.h>
#include <linux/cpuhotplug.h>
#include <linux/platform_device.h>
#include <linux/property.h>
#include <linux/dma-mapping.h>
#include <linux/scatterlist.h>
#include <linux/ioport.h>
#include <linux/io.h>
#include <linux/pci.h>
#include <linux/fs.h>
#include <linux/seq_file.h>
#include <linux/uio.h>
#include <linux/mm.h>
#include <linux/vmalloc.h>
#include <linux/random.h>
#include <linux/utsname.h>
#include <linux/component.h>
#include <linux/moduleparam.h>
#include <linux/cred.h>
#include <linux/pid.h>
#include <linux/sched/signal.h>
#include <linux/timekeeping.h>
#include <linux/sysfs.h>
#include <linux/kernfs.h>
#include <linux/module.h>
#include "nova.h"

/* ---------------------------------------------------------------- slab caches */

struct kmem_cache *__kmem_cache_create_args(const char *name, unsigned int object_size,
					    struct kmem_cache_args *args, slab_flags_t flags)
{
	struct kmem_cache *s = kzalloc(sizeof(*s), GFP_KERNEL);

	if (!s)
		return NULL;
	s->name = name;
	s->size = object_size;
	s->align = args && args->align ? args->align : ARCH_KMALLOC_MINALIGN;
	if ((flags & SLAB_HWCACHE_ALIGN) && s->align < L1_CACHE_BYTES)
		s->align = L1_CACHE_BYTES;
	s->ctor = args ? args->ctor : NULL;
	return s;
}

void *kmem_cache_alloc_noprof(struct kmem_cache *s, gfp_t flags)
{
	void *obj = nova_alloc(s->size, s->align < 16 ? 16 : s->align, (flags & __GFP_ZERO) || !s->ctor,
			       flags & (GFP_DMA32 | GFP_DMA));

	if (obj && s->ctor && !(flags & __GFP_ZERO))
		s->ctor(obj);
	return obj;
}

void *kmem_cache_alloc_lru_noprof(struct kmem_cache *s, struct list_lru *lru, gfp_t gfpflags)
{
	return kmem_cache_alloc_noprof(s, gfpflags);
}

void *kmem_cache_alloc_node_noprof(struct kmem_cache *s, gfp_t flags, int node)
{
	return kmem_cache_alloc_noprof(s, flags);
}

void kmem_cache_free(struct kmem_cache *s, void *objp)
{
	if (objp)
		nova_free(objp);
}

void kmem_cache_destroy(struct kmem_cache *s)
{
	kfree(s);
}

int kmem_cache_shrink(struct kmem_cache *s)
{
	return 0;
}

void *krealloc_node_align_noprof(const void *objp, DECL_TOKEN_PARAMS(new_size, token), unsigned long align,
				 gfp_t flags, int nid)
{
	size_t old = ZERO_OR_NULL_PTR(objp) ? 0 : nova_alloc_size(objp);
	void *p;

	if (!new_size) {
		kfree(objp);
		return ZERO_SIZE_PTR;
	}
	if (old >= new_size)
		return (void *)objp;
	p = nova_alloc(new_size, align < 16 ? 16 : align, flags & __GFP_ZERO, false);
	if (p && old)
		memcpy(p, objp, old);
	if (p)
		kfree(objp);
	return p;
}

void *vzalloc_node_noprof(unsigned long size, int node)
{
	return nova_alloc(size, PAGE_SIZE, true, false);
}

void *memdup_user(const void __user *src, size_t len)
{
	void *p = kmalloc(len, GFP_KERNEL);

	if (!p)
		return ERR_PTR(-ENOMEM);
	memcpy(p, (const void __force *)src, len);
	return p;
}

/* Every allocation is in the direct map, so there are no vmalloc or module addresses. */
int is_vmalloc_or_module_addr(const void *x)
{
	return 0;
}

struct page *vmalloc_to_page(const void *addr)
{
	return virt_to_page(addr);
}

/* Static keys for the slab hardening options, both off. */
DEFINE_STATIC_KEY_FALSE(init_on_alloc);
DEFINE_STATIC_KEY_FALSE(init_on_free);

/* ---------------------------------------------------------------- RCU callbacks */

/* Run after the current poll round: no reader can still be in a read-side section then. */
static struct rcu_head *lx_rcu_list;

void call_rcu(struct rcu_head *head, rcu_callback_t func)
{
	unsigned long flags;

	head->func = func;
	local_irq_save(flags);
	head->next = lx_rcu_list;
	lx_rcu_list = head;
	local_irq_restore(flags);
}

void lx_emul_rcu_run(void)
{
	struct rcu_head *list;
	unsigned long flags;

	local_irq_save(flags);
	list = lx_rcu_list;
	lx_rcu_list = NULL;
	local_irq_restore(flags);
	while (list) {
		struct rcu_head *next = list->next;

		list->func(list);
		list = next;
	}
}

void rcu_barrier(void)
{
	lx_emul_rcu_run();
}

/* ---------------------------------------------------------------- CPU hotplug */

/* One CPU, always online: run the startup callback for it right away. */
int __cpuhp_setup_state(enum cpuhp_state state, const char *name, bool invoke, int (*startup)(unsigned int cpu),
			int (*teardown)(unsigned int cpu), bool multi_instance)
{
	if (invoke && startup)
		startup(0);
	return state == CPUHP_AP_ONLINE_DYN ? CPUHP_AP_ONLINE_DYN : 0;
}

/* ---------------------------------------------------------------- platform devices */

const struct bus_type platform_bus_type = { .name = "platform" };

struct lx_platform_object {
	struct platform_device pdev;
	char name[];
};

static void lx_platform_release(struct device *dev)
{
	struct lx_platform_object *pa = container_of(dev, struct lx_platform_object, pdev.dev);

	kfree(pa->pdev.resource);
	kfree(pa);
}

struct platform_device *platform_device_alloc(const char *name, int id)
{
	struct lx_platform_object *pa = kzalloc(sizeof(*pa) + strlen(name) + 1, GFP_KERNEL);

	if (!pa)
		return NULL;
	strcpy(pa->name, name);
	pa->pdev.name = pa->name;
	pa->pdev.id = id;
	device_initialize(&pa->pdev.dev);
	pa->pdev.dev.release = lx_platform_release;
	return &pa->pdev;
}

int platform_device_add_resources(struct platform_device *pdev, const struct resource *res, unsigned int num)
{
	struct resource *r = res ? kmemdup(res, sizeof(*res) * num, GFP_KERNEL) : NULL;

	if (res && !r)
		return -ENOMEM;
	kfree(pdev->resource);
	pdev->resource = r;
	pdev->num_resources = num;
	return 0;
}

int platform_device_add(struct platform_device *pdev)
{
	pdev->dev.bus = &platform_bus_type;
	if (pdev->id == PLATFORM_DEVID_NONE)
		dev_set_name(&pdev->dev, "%s", pdev->name);
	else
		dev_set_name(&pdev->dev, "%s.%d", pdev->name, pdev->id);
	return device_add(&pdev->dev);
}

void platform_device_del(struct platform_device *pdev)
{
	device_del(&pdev->dev);
}

void platform_device_put(struct platform_device *pdev)
{
	if (pdev)
		put_device(&pdev->dev);
}

void platform_device_unregister(struct platform_device *pdev)
{
	platform_device_del(pdev);
	platform_device_put(pdev);
}

/* ---------------------------------------------------------------- firmware nodes */

/* Nova has no device tree and no ACPI device properties for drivers yet. */
const struct fwnode_handle *__dev_fwnode_const(const struct device *dev)
{
	return dev->fwnode;
}

bool device_property_read_bool(const struct device *dev, const char *propname)
{
	return false;
}

int device_property_read_string(const struct device *dev, const char *propname, const char **val)
{
	return -EINVAL;
}

int fwnode_property_match_property_string(const struct fwnode_handle *fwnode, const char *propname,
					  const char * const *array, size_t n)
{
	return -EINVAL;
}

void device_set_node(struct device *dev, struct fwnode_handle *fwnode)
{
	dev->fwnode = fwnode;
}

void device_set_of_node_from_dev(struct device *dev, const struct device *dev2)
{
}

int device_create_managed_software_node(struct device *dev, const struct property_entry *properties,
					const struct software_node *parent)
{
	return 0;
}

void device_link_del(struct device_link *link)
{
}

void component_del(struct device *dev, const struct component_ops *ops)
{
}

/* ---------------------------------------------------------------- resources and I/O */

struct resource ioport_resource = { .name = "PCI IO", .start = 0, .end = IO_SPACE_LIMIT, .flags = IORESOURCE_IO };
struct resource iomem_resource = { .name = "PCI mem", .start = 0, .end = -1, .flags = IORESOURCE_MEM };

/* Nova gives each region to exactly one driver: requests succeed, nothing is tracked. */
struct resource *__devm_request_region(struct device *dev, struct resource *parent, resource_size_t start,
				       resource_size_t n, const char *name)
{
	struct resource *res = devm_kzalloc(dev, sizeof(*res), GFP_KERNEL);

	if (res) {
		res->start = start;
		res->end = start + n - 1;
		res->name = name;
		res->flags = parent->flags;
	}
	return res;
}

void __iomem *devm_ioremap(struct device *dev, resource_size_t offset, resource_size_t size)
{
	return ioremap(offset, size);
}

void *devm_memremap(struct device *dev, resource_size_t offset, size_t size, unsigned long flags)
{
	return (void __force *)ioremap(offset, size);
}

void __iomem *pci_ioremap_bar(struct pci_dev *pdev, int bar)
{
	if (!(pci_resource_flags(pdev, bar) & IORESOURCE_MEM))
		return NULL;
	return ioremap(pci_resource_start(pdev, bar), pci_resource_len(pdev, bar));
}

void pci_clear_and_set_config_dword(const struct pci_dev *dev, int pos, u32 clear, u32 set)
{
	u32 val;

	pci_read_config_dword(dev, pos, &val);
	pci_write_config_dword(dev, pos, (val & ~clear) | set);
}

pci_power_t pci_choose_state(struct pci_dev *dev, pm_message_t state)
{
	return PCI_D0;
}

void native_io_delay(void)
{
	asm volatile("outb %al, $0x80");
}

/* ---------------------------------------------------------------- scatter-gather DMA */

unsigned int dma_map_sg_attrs(struct device *dev, struct scatterlist *sg, int nents, enum dma_data_direction dir,
			      unsigned long attrs)
{
	struct scatterlist *s;
	int i;

	for_each_sg(sg, s, nents, i) {
		s->dma_address = sg_phys(s);
		sg_dma_len(s) = s->length;
	}
	return nents;
}

void dma_unmap_sg_attrs(struct device *dev, struct scatterlist *sg, int nents, enum dma_data_direction dir,
			unsigned long attrs)
{
}

void __dma_sync_sg_for_cpu(struct device *dev, struct scatterlist *sg, int nelems, enum dma_data_direction dir)
{
}

void __dma_sync_sg_for_device(struct device *dev, struct scatterlist *sg, int nelems, enum dma_data_direction dir)
{
}

void *dmam_alloc_attrs(struct device *dev, size_t size, dma_addr_t *dma_handle, gfp_t gfp, unsigned long attrs)
{
	return dma_alloc_attrs(dev, size, dma_handle, gfp, attrs);
}

/* "Non-contiguous" buffers are simply contiguous here, described by one scatterlist entry. */
struct lx_noncontig {
	struct sg_table sgt;
	struct scatterlist sg;
	void *cpu;
};

struct sg_table *dma_alloc_noncontiguous(struct device *dev, size_t size, enum dma_data_direction dir, gfp_t gfp,
					 unsigned long attrs)
{
	struct lx_noncontig *nc = kzalloc(sizeof(*nc), GFP_KERNEL);
	dma_addr_t dma;

	if (!nc)
		return NULL;
	nc->cpu = dma_alloc_attrs(dev, size, &dma, gfp, 0);
	if (!nc->cpu) {
		kfree(nc);
		return NULL;
	}
	sg_init_table(&nc->sg, 1);
	sg_set_buf(&nc->sg, nc->cpu, size);
	nc->sg.dma_address = dma;
	sg_dma_len(&nc->sg) = size;
	nc->sgt.sgl = &nc->sg;
	nc->sgt.nents = nc->sgt.orig_nents = 1;
	return &nc->sgt;
}

void dma_free_noncontiguous(struct device *dev, size_t size, struct sg_table *sgt, enum dma_data_direction dir)
{
	struct lx_noncontig *nc = container_of(sgt, struct lx_noncontig, sgt);

	dma_free_attrs(dev, size, nc->cpu, nc->sg.dma_address, 0);
	kfree(nc);
}

void *dma_vmap_noncontiguous(struct device *dev, size_t size, struct sg_table *sgt)
{
	return container_of(sgt, struct lx_noncontig, sgt)->cpu;
}

void dma_vunmap_noncontiguous(struct device *dev, void *vaddr)
{
}

int dma_mmap_attrs(struct device *dev, struct vm_area_struct *vma, void *cpu_addr, dma_addr_t dma_addr, size_t size,
		   unsigned long attrs)
{
	return -ENXIO;
}

/* ---------------------------------------------------------------- VFS, processes, misc */

/* usbfs (/dev/bus/usb) and other character-device paths are reachable only once Nova has
 * a device filesystem; these keep the code linkable and refuse politely. */
struct timespec64 current_time(struct inode *inode)
{
	return ns_to_timespec64(nova_nanos());
}

struct timespec64 inode_set_ctime_current(struct inode *inode)
{
	return current_time(inode);
}

char *d_path(const struct path *path, char *buf, int buflen)
{
	return ERR_PTR(-ENAMETOOLONG);
}

char *mangle_path(char *s, const char *p, const char *esc)
{
	return NULL;
}

int seq_write(struct seq_file *seq, const void *data, size_t len)
{
	return -1;
}

loff_t noop_llseek(struct file *file, loff_t offset, int whence)
{
	return file->f_pos;
}

loff_t no_seek_end_llseek(struct file *file, loff_t offset, int whence)
{
	return -EINVAL;
}

void iov_iter_advance(struct iov_iter *i, size_t bytes)
{
	i->count -= min(bytes, i->count);
}

ssize_t iov_iter_extract_pages(struct iov_iter *i, struct page ***pages, size_t maxsize, unsigned int maxpages,
			       iov_iter_extraction_t extraction_flags, size_t *offset0)
{
	return -EFAULT;
}

void unpin_user_page(struct page *page)
{
}

int remap_pfn_range(struct vm_area_struct *vma, unsigned long addr, unsigned long pfn, unsigned long size,
		    pgprot_t pgprot)
{
	return -EINVAL;
}

int kill_pid_usb_asyncio(int sig, int errno, sigval_t addr, struct pid *pid, const struct cred *cred)
{
	return -ESRCH;
}

void __put_cred(struct cred *cred)
{
}

void put_pid(struct pid *pid)
{
}

ktime_t ktime_mono_to_any(ktime_t tmono, enum tk_offsets offs)
{
	return tmono;
}

/* Nova's RNG does not take entropy from drivers. */
void add_device_randomness(const void *buf, size_t len)
{
}

void add_input_randomness(unsigned int type, unsigned int code, unsigned int value)
{
}

int __printk_ratelimit(const char *func)
{
	return 1;
}

int _printk_deferred(const char *fmt, ...)
{
	va_list args;
	int n;

	va_start(args, fmt);
	n = vprintk(fmt, args);
	va_end(args);
	return n;
}

void _dev_printk(const char *level, const struct device *dev, const char *fmt, ...)
{
	struct va_format vaf;
	va_list args;

	va_start(args, fmt);
	vaf.fmt = fmt;
	vaf.va = &args;
	_printk("%s%s %s: %pV", level, dev && dev->driver ? dev->driver->name : "", dev ? dev_name(dev) : "", &vaf);
	va_end(args);
}

struct uts_namespace init_uts_ns = {
	.name = {
		.sysname = "Nova",
		.nodename = "nova",
		.release = "7.2.9-nova",
		.version = "#1",
		.machine = "x86_64",
	},
};

/* Module parameters (string flavours): built-in drivers keep their defaults. */
const struct kernel_param_ops param_ops_ullong;

int param_set_copystring(const char *val, const struct kernel_param *kp)
{
	return 0;
}

int param_get_string(char *buffer, const struct kernel_param *kp)
{
	return 0;
}

/* ---------------------------------------------------------------- sysfs */

/*
 * There is no sysfs tree: attribute groups and links are accepted and ignored, and a node
 * lookup returns a placeholder (code such as the USB hub driver treats a missing node as
 * an error, but only passes the node back to kernfs_notify/kernfs_put).
 */
static struct kernfs_node *lx_kernfs_placeholder = (struct kernfs_node *)&lx_kernfs_placeholder;

struct kernfs_node *kernfs_find_and_get_ns(struct kernfs_node *parent, const char *name, const struct ns_common *ns)
{
	return lx_kernfs_placeholder;
}

void kernfs_notify(struct kernfs_node *kn)
{
}

void kernfs_put(struct kernfs_node *kn)
{
}

int sysfs_create_group(struct kobject *kobj, const struct attribute_group *grp)
{
	return 0;
}

int sysfs_update_group(struct kobject *kobj, const struct attribute_group *grp)
{
	return 0;
}

void sysfs_remove_group(struct kobject *kobj, const struct attribute_group *grp)
{
}

int sysfs_create_link(struct kobject *kobj, struct kobject *target, const char *name)
{
	return 0;
}

void sysfs_remove_link(struct kobject *kobj, const char *name)
{
}

void sysfs_notify(struct kobject *kobj, const char *dir, const char *attr)
{
}

struct kernfs_node *sysfs_break_active_protection(struct kobject *kobj, const struct attribute *attr)
{
	return lx_kernfs_placeholder;
}

void sysfs_unbreak_active_protection(struct kernfs_node *kn)
{
}

/* Attribute "show" helpers, should anything read an attribute in-kernel. */
int sysfs_emit(char *buf, const char *fmt, ...)
{
	va_list args;
	int len;

	va_start(args, fmt);
	len = vscnprintf(buf, PAGE_SIZE, fmt, args);
	va_end(args);
	return len;
}

int sysfs_emit_at(char *buf, int at, const char *fmt, ...)
{
	va_list args;
	int len;

	if (at < 0 || at >= PAGE_SIZE)
		return 0;
	va_start(args, fmt);
	len = vscnprintf(buf + at, PAGE_SIZE - at, fmt, args);
	va_end(args);
	return len;
}

ssize_t __modver_version_show(const struct module_attribute *mattr, struct module_kobject *mk, char *buf)
{
	return 0;
}

int component_add(struct device *dev, const struct component_ops *ops)
{
	return 0;
}

/* kfree_rcu()/kvfree_rcu(): free `ptr` after a grace period (with a NULL head, after
 * waiting for one now). The object is freed from an RCU callback on a small record. */
struct lx_kvfree_rcu {
	struct rcu_head rcu;
	void *ptr;
};

static void lx_kvfree_rcu_cb(struct rcu_head *head)
{
	struct lx_kvfree_rcu *r = container_of(head, struct lx_kvfree_rcu, rcu);

	kvfree(r->ptr);
	kfree(r);
}

void kvfree_call_rcu(struct rcu_head *head, void *ptr)
{
	struct lx_kvfree_rcu *r = kmalloc(sizeof(*r), GFP_ATOMIC);

	if (!head || !r) {
		kfree(r);
		synchronize_rcu();
		kvfree(ptr);
		return;
	}
	r->ptr = ptr;
	call_rcu(&r->rcu, lx_kvfree_rcu_cb);
}
