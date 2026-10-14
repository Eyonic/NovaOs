// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul PCI: Linux's view of the PCI functions Nova hands over (those its own Rust
 * drivers did not claim), driver registration and matching, configuration space, BAR
 * resources, ioremap and DMA.
 *
 * Nova enumerates the bus (kernel/src/pci.rs) and calls nova_lx_pci_add() per unclaimed
 * function before running the initcalls, so __pci_register_driver() can probe right away,
 * as it does on Linux for devices found at boot.
 */
#include <linux/pci.h>
#include <linux/dma-mapping.h>
#include <linux/io.h>
#include <linux/init.h>
#include <linux/slab.h>
#include <linux/list.h>
#include "nova.h"

const struct bus_type pci_bus_type = { .name = "pci" };

/* Every function handed over, in bus order (pci_dev's own bus_list links it into its bus). */
struct lx_pci_dev {
	struct pci_dev pdev;
	struct list_head node;
};
static LIST_HEAD(lx_pci_devices);
#define for_each_lx_pci_dev(d) list_for_each_entry(d, &lx_pci_devices, node)
static struct pci_bus *lx_pci_buses[256];

static struct pci_bus *lx_pci_bus(u8 number)
{
	if (!lx_pci_buses[number]) {
		struct pci_bus *bus = kzalloc(sizeof(*bus), GFP_KERNEL);

		bus->number = number;
		INIT_LIST_HEAD(&bus->devices);
		lx_pci_buses[number] = bus;
	}
	return lx_pci_buses[number];
}

/* ---------------------------------------------------------------- configuration space */

#define LX_PCI_CONFIG(type, width, name)						\
	int pci_read_config_##name(const struct pci_dev *dev, int where, type *val)	\
	{										\
		if (where < 0 || where + width > 256) {					\
			*val = (type)~0;						\
			return PCIBIOS_BAD_REGISTER_NUMBER;				\
		}									\
		*val = nova_pci_read(dev->bus->number, dev->devfn, where, width);	\
		return PCIBIOS_SUCCESSFUL;						\
	}										\
	int pci_write_config_##name(const struct pci_dev *dev, int where, type val)	\
	{										\
		if (where < 0 || where + width > 256)					\
			return PCIBIOS_BAD_REGISTER_NUMBER;				\
		nova_pci_write(dev->bus->number, dev->devfn, where, width, val);	\
		return PCIBIOS_SUCCESSFUL;						\
	}

LX_PCI_CONFIG(u8, 1, byte)
LX_PCI_CONFIG(u16, 2, word)
LX_PCI_CONFIG(u32, 4, dword)

u8 pci_find_capability(struct pci_dev *dev, int cap)
{
	u16 status;
	u8 pos, id;
	int ttl = 48;

	pci_read_config_word(dev, PCI_STATUS, &status);
	if (!(status & PCI_STATUS_CAP_LIST))
		return 0;
	pci_read_config_byte(dev, PCI_CAPABILITY_LIST, &pos);
	while (pos >= 0x40 && ttl--) {
		pos &= ~3;
		pci_read_config_byte(dev, pos + PCI_CAP_LIST_ID, &id);
		if (id == 0xff)
			break;
		if (id == cap)
			return pos;
		pci_read_config_byte(dev, pos + PCI_CAP_LIST_NEXT, &pos);
	}
	return 0;
}

/* Extended capabilities live above 256 bytes, which the legacy configuration mechanism
 * cannot reach; ECAM (from ACPI MCFG) arrives with real-hardware support. */
u16 pci_find_ext_capability(struct pci_dev *dev, int cap)
{
	return 0;
}

/* ---------------------------------------------------------------- resources */

/* Size BAR i the way Linux's __pci_read_base does: decoding off, write all ones,
 * read back the mask, restore. Returns the number of BAR slots used (2 for 64-bit). */
static int lx_pci_read_bar(struct pci_dev *dev, int i, u16 command)
{
	struct resource *res = &dev->resource[i];
	int reg = PCI_BASE_ADDRESS_0 + i * 4;
	u32 lo, lo_mask, hi = 0, hi_mask = ~0u;
	u64 base, mask;

	pci_read_config_dword(dev, reg, &lo);
	pci_write_config_dword(dev, reg, ~0u);
	pci_read_config_dword(dev, reg, &lo_mask);
	pci_write_config_dword(dev, reg, lo);
	if (!lo_mask || lo_mask == ~0u)
		return 1;

	if (lo & PCI_BASE_ADDRESS_SPACE_IO) {
		base = lo & PCI_BASE_ADDRESS_IO_MASK;
		mask = (lo_mask & PCI_BASE_ADDRESS_IO_MASK) | 0xffff0000u;
		res->flags = IORESOURCE_IO;
		res->start = base;
		res->end = base + (u32)(~mask + 1) - 1;
		res->name = pci_name(dev);
		return 1;
	}

	res->flags = IORESOURCE_MEM;
	if (lo & PCI_BASE_ADDRESS_MEM_PREFETCH)
		res->flags |= IORESOURCE_PREFETCH;
	if ((lo & PCI_BASE_ADDRESS_MEM_TYPE_MASK) == PCI_BASE_ADDRESS_MEM_TYPE_64) {
		pci_read_config_dword(dev, reg + 4, &hi);
		pci_write_config_dword(dev, reg + 4, ~0u);
		pci_read_config_dword(dev, reg + 4, &hi_mask);
		pci_write_config_dword(dev, reg + 4, hi);
		res->flags |= IORESOURCE_MEM_64;
	}
	base = ((u64)hi << 32) | (lo & PCI_BASE_ADDRESS_MEM_MASK);
	mask = ((u64)hi_mask << 32) | (lo_mask & PCI_BASE_ADDRESS_MEM_MASK);
	if (!base) {
		res->flags = 0;
	} else {
		res->start = base;
		res->end = base + (~mask + 1) - 1;
		res->name = pci_name(dev);
	}
	return res->flags & IORESOURCE_MEM_64 ? 2 : 1;
}

static void lx_pci_read_bases(struct pci_dev *dev)
{
	u16 command;
	int i;

	pci_read_config_word(dev, PCI_COMMAND, &command);
	pci_write_config_word(dev, PCI_COMMAND, command & ~(PCI_COMMAND_IO | PCI_COMMAND_MEMORY));
	for (i = 0; i < PCI_STD_NUM_BARS;)
		i += lx_pci_read_bar(dev, i, command);
	pci_write_config_word(dev, PCI_COMMAND, command);
}

int pci_enable_device(struct pci_dev *dev)
{
	u16 command, wanted = 0;
	int i;

	for (i = 0; i < PCI_STD_NUM_BARS; i++) {
		if (dev->resource[i].flags & IORESOURCE_IO)
			wanted |= PCI_COMMAND_IO;
		if (dev->resource[i].flags & IORESOURCE_MEM)
			wanted |= PCI_COMMAND_MEMORY;
	}
	pci_read_config_word(dev, PCI_COMMAND, &command);
	if ((command & wanted) != wanted)
		pci_write_config_word(dev, PCI_COMMAND, command | wanted);
	atomic_inc(&dev->enable_cnt);
	dev->current_state = PCI_D0;
	return 0;
}

int pci_enable_device_mem(struct pci_dev *dev)
{
	return pci_enable_device(dev);
}

int pcim_enable_device(struct pci_dev *pdev)
{
	return pci_enable_device(pdev);
}

void pci_disable_device(struct pci_dev *dev)
{
	if (atomic_dec_return(&dev->enable_cnt) == 0)
		pci_clear_master(dev);
}

static void lx_pci_master(struct pci_dev *dev, bool enable)
{
	u16 command, wanted;

	pci_read_config_word(dev, PCI_COMMAND, &command);
	wanted = enable ? command | PCI_COMMAND_MASTER : command & ~PCI_COMMAND_MASTER;
	if (wanted != command)
		pci_write_config_word(dev, PCI_COMMAND, wanted);
	dev->is_busmaster = enable;
}

void pci_set_master(struct pci_dev *dev)
{
	lx_pci_master(dev, true);
}

void pci_clear_master(struct pci_dev *dev)
{
	lx_pci_master(dev, false);
}

static void lx_pci_command(struct pci_dev *dev, u16 set, u16 clear)
{
	u16 command;

	pci_read_config_word(dev, PCI_COMMAND, &command);
	pci_write_config_word(dev, PCI_COMMAND, (command & ~clear) | set);
}

int pci_set_mwi(struct pci_dev *dev)
{
	lx_pci_command(dev, PCI_COMMAND_INVALIDATE, 0);
	return 0;
}

void pci_clear_mwi(struct pci_dev *dev)
{
	lx_pci_command(dev, 0, PCI_COMMAND_INVALIDATE);
}

/* Power management: devices stay in D0, as at boot; wake-up is not wired. */
int pci_set_power_state(struct pci_dev *dev, pci_power_t state)
{
	dev->current_state = state;
	return 0;
}

int pci_enable_wake(struct pci_dev *dev, pci_power_t state, bool enable)
{
	return 0;
}

/* Nova gives each function to exactly one driver, so region reservations always succeed. */
int pci_request_regions(struct pci_dev *dev, const char *name)
{
	return 0;
}

int pci_request_region(struct pci_dev *dev, int bar, const char *name)
{
	return 0;
}

void pci_release_regions(struct pci_dev *dev)
{
}

void pci_release_region(struct pci_dev *dev, int bar)
{
}

struct pci_dev *pci_dev_get(struct pci_dev *dev)
{
	return dev;
}

void pci_dev_put(struct pci_dev *dev)
{
}

struct device *get_device(struct device *dev)
{
	return dev;
}

void put_device(struct device *dev)
{
}

/* ---------------------------------------------------------------- MMIO */

void __iomem *ioremap(resource_size_t offset, unsigned long size)
{
	return (void __iomem *)nova_map_mmio(offset, size, false);
}

void __iomem *ioremap_uc(resource_size_t offset, unsigned long size)
{
	return ioremap(offset, size);
}

void __iomem *ioremap_wc(resource_size_t offset, unsigned long size)
{
	return (void __iomem *)nova_map_mmio(offset, size, true);
}

void __iomem *ioremap_cache(resource_size_t offset, unsigned long size)
{
	return ioremap(offset, size);
}

/* Nova never removes MMIO mappings (memory.rs). */
void iounmap(volatile void __iomem *addr)
{
}

/* ---------------------------------------------------------------- DMA */

/* Nova has no IOMMU: bus addresses are physical addresses. Memory below 4 GiB comes from
 * a reserved pool (memory.rs), for devices whose mask is 32 bits (Linux's default). */
static bool lx_dma32(struct device *dev, u64 mask)
{
	return mask && mask < (1ULL << 40);
}

int dma_set_mask(struct device *dev, u64 mask)
{
	if (!dev->dma_mask)
		return -EIO;
	*dev->dma_mask = mask;
	return 0;
}

int dma_set_coherent_mask(struct device *dev, u64 mask)
{
	dev->coherent_dma_mask = mask;
	return 0;
}

void *dma_alloc_attrs(struct device *dev, size_t size, dma_addr_t *dma_handle, gfp_t flag,
		      unsigned long attrs)
{
	u64 mask = dev ? dev->coherent_dma_mask : DMA_BIT_MASK(32);
	void *cpu = nova_alloc(PAGE_ALIGN(size), PAGE_SIZE, true, lx_dma32(dev, mask));

	if (!cpu)
		return NULL;
	*dma_handle = virt_to_phys(cpu);
	if (mask && *dma_handle + size - 1 > mask) {
		nova_free(cpu);
		return NULL;
	}
	return cpu;
}

void dma_free_attrs(struct device *dev, size_t size, void *cpu_addr, dma_addr_t dma_handle,
		    unsigned long attrs)
{
	if (cpu_addr)
		nova_free(cpu_addr);
}

dma_addr_t dma_map_page_attrs(struct device *dev, struct page *page, size_t offset, size_t size,
			      enum dma_data_direction dir, unsigned long attrs)
{
	dma_addr_t addr = page_to_phys(page) + offset;
	u64 mask = dev && dev->dma_mask ? *dev->dma_mask : DMA_BIT_MASK(32);

	/* No bounce buffers yet: memory beyond the device's reach is a mapping error,
	 * which drivers check with dma_mapping_error(). */
	if (addr + size - 1 > mask)
		return DMA_MAPPING_ERROR;
	return addr;
}

void dma_unmap_page_attrs(struct device *dev, dma_addr_t addr, size_t size, enum dma_data_direction dir,
			  unsigned long attrs)
{
}

dma_addr_t dma_map_phys(struct device *dev, phys_addr_t phys, size_t size,
			enum dma_data_direction dir, unsigned long attrs)
{
	return phys;
}

void dma_unmap_phys(struct device *dev, dma_addr_t addr, size_t size, enum dma_data_direction dir,
		    unsigned long attrs)
{
}

/* ---------------------------------------------------------------- drivers */

static struct pci_driver *lx_pci_drivers[64];

static const struct pci_device_id *lx_pci_match(const struct pci_device_id *ids, struct pci_dev *dev)
{
	for (; ids && (ids->vendor || ids->subvendor || ids->class_mask); ids++) {
		if ((ids->vendor == PCI_ANY_ID || ids->vendor == dev->vendor) &&
		    (ids->device == PCI_ANY_ID || ids->device == dev->device) &&
		    (ids->subvendor == PCI_ANY_ID || ids->subvendor == dev->subsystem_vendor) &&
		    (ids->subdevice == PCI_ANY_ID || ids->subdevice == dev->subsystem_device) &&
		    !((ids->class ^ dev->class) & ids->class_mask) && !ids->override_only)
			return ids;
	}
	return NULL;
}

const struct pci_device_id *pci_match_id(const struct pci_device_id *ids, struct pci_dev *dev)
{
	return lx_pci_match(ids, dev);
}

static void lx_pci_probe(struct pci_driver *drv, struct pci_dev *dev)
{
	const struct pci_device_id *id = lx_pci_match(drv->id_table, dev);
	int err;

	if (!id || dev->driver)
		return;
	dev->dev.driver = &drv->driver;
	err = drv->probe(dev, id);
	if (err) {
		dev->dev.driver = NULL;
		if (err != -ENODEV)
			pr_warn("%s %s: probe failed (%d)\n", drv->name, pci_name(dev), err);
		return;
	}
	dev->driver = drv;
}

int __pci_register_driver(struct pci_driver *drv, struct module *owner, const char *mod_name)
{
	struct lx_pci_dev *d;
	unsigned int i;

	drv->driver.name = drv->name;
	drv->driver.bus = &pci_bus_type;
	for (i = 0; i < ARRAY_SIZE(lx_pci_drivers) && lx_pci_drivers[i]; i++)
		;
	if (i == ARRAY_SIZE(lx_pci_drivers))
		return -ENOMEM;
	lx_pci_drivers[i] = drv;
	for_each_lx_pci_dev(d)
		lx_pci_probe(drv, &d->pdev);
	return 0;
}

void pci_unregister_driver(struct pci_driver *drv)
{
	struct lx_pci_dev *d;

	for_each_lx_pci_dev(d) {
		struct pci_dev *dev = &d->pdev;

		if (dev->driver != drv)
			continue;
		if (drv->remove)
			drv->remove(dev);
		dev->driver = NULL;
		dev->dev.driver = NULL;
	}
	for (unsigned int i = 0; i < ARRAY_SIZE(lx_pci_drivers); i++)
		if (lx_pci_drivers[i] == drv)
			lx_pci_drivers[i] = NULL;
}

struct pci_dev *pci_get_device(unsigned int vendor, unsigned int device, struct pci_dev *from)
{
	struct lx_pci_dev *d = from ? list_next_entry(container_of(from, struct lx_pci_dev, pdev), node)
				    : list_first_entry(&lx_pci_devices, struct lx_pci_dev, node);

	for (; !list_entry_is_head(d, &lx_pci_devices, node); d = list_next_entry(d, node))
		if ((vendor == PCI_ANY_ID || d->pdev.vendor == vendor) &&
		    (device == PCI_ANY_ID || d->pdev.device == device))
			return &d->pdev;
	return NULL;
}

/* ---------------------------------------------------------------- bridge from Nova */

void nova_lx_pci_add(u8 busnr, u8 devfn, u16 vendor, u16 device, u16 subvendor, u16 subdevice,
		     u32 class_rev, u8 irq)
{
	struct lx_pci_dev *d = kzalloc(sizeof(*d), GFP_KERNEL);
	struct pci_dev *dev;
	u8 hdr;

	if (!d)
		return;
	dev = &d->pdev;
	dev->bus = lx_pci_bus(busnr);
	dev->devfn = devfn;
	dev->vendor = vendor;
	dev->device = device;
	dev->subsystem_vendor = subvendor;
	dev->subsystem_device = subdevice;
	dev->class = class_rev >> 8;
	dev->revision = class_rev & 0xff;
	dev->irq = irq;
	dev->current_state = PCI_UNKNOWN;
	dev->dma_mask = DMA_BIT_MASK(32);
	dev->dev.dma_mask = &dev->dma_mask;
	dev->dev.coherent_dma_mask = DMA_BIT_MASK(32);
	dev->dev.bus = &pci_bus_type;
	dev->dev.init_name = kasprintf(GFP_KERNEL, "0000:%02x:%02x.%d", busnr, PCI_SLOT(devfn), PCI_FUNC(devfn));
	pci_read_config_byte(dev, PCI_HEADER_TYPE, &hdr);
	dev->hdr_type = hdr & 0x7f;
	pci_read_config_byte(dev, PCI_INTERRUPT_PIN, &dev->pin);
	if (dev->hdr_type == PCI_HEADER_TYPE_NORMAL)
		lx_pci_read_bases(dev);
	list_add_tail(&d->node, &lx_pci_devices);
	list_add_tail(&dev->bus_list, &dev->bus->devices);
}

const char *nova_lx_pci_driver(u8 busnr, u8 devfn)
{
	struct lx_pci_dev *d;

	for_each_lx_pci_dev(d)
		if (d->pdev.bus->number == busnr && d->pdev.devfn == devfn)
			return d->pdev.driver ? d->pdev.driver->name : NULL;
	return NULL;
}

/* Built-in drivers register themselves through initcalls (module_init and friends
 * become device_initcall entries); the linker script collects them in level order. */
extern initcall_entry_t __nova_initcall_start[], __nova_initcall_end[];

void nova_lx_start(void)
{
	initcall_entry_t *entry;

	for (entry = __nova_initcall_start; entry < __nova_initcall_end; entry++) {
		initcall_t fn = (initcall_t)offset_to_ptr(entry);
		int ret = fn();

		if (ret && ret != -ENODEV)
			pr_warn("initcall %ps failed: %d\n", fn, ret);
	}
}
