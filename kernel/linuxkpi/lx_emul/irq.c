// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul IRQ domains: virtual interrupt numbers that a driver demultiplexes itself.
 *
 * amdgpu has one interrupt (MSI) for the whole GPU and decodes its source from the
 * interrupt ring; sources that belong to child devices (the audio co-processor) are
 * forwarded through an IRQ domain, whose mappings give each source a Linux IRQ number
 * the child requests with request_irq(). Here a mapping is a struct irq_desc with the
 * flow handler the domain's map() installed; handle_simple_irq() runs the handlers
 * registered on that number (sched.c). Virtual numbers start at LX_VIRQ_BASE, apart from
 * GSIs and MSI numbers (0x1000 + vector).
 */
#include <linux/irq.h>
#include <linux/irqdesc.h>
#include <linux/irqdomain.h>
#include <linux/interrupt.h>
#include <linux/property.h>
#include <linux/slab.h>
#include "nova.h"

#define LX_VIRQ_BASE 0x2000
#define LX_VIRQS 256

struct lx_virq {
	struct irq_domain *domain;
	irq_hw_number_t hwirq;
	struct irq_desc desc;
};

static struct lx_virq lx_virqs[LX_VIRQS];

static struct lx_virq *lx_virq(unsigned int irq)
{
	if (irq < LX_VIRQ_BASE || irq >= LX_VIRQ_BASE + LX_VIRQS || !lx_virqs[irq - LX_VIRQ_BASE].domain)
		return NULL;
	return &lx_virqs[irq - LX_VIRQ_BASE];
}

struct irq_domain *irq_domain_instantiate(const struct irq_domain_info *info)
{
	struct irq_domain *domain = kzalloc(sizeof(*domain), GFP_KERNEL);
	int err;

	if (!domain)
		return ERR_PTR(-ENOMEM);
	domain->ops = info->ops;
	domain->host_data = info->host_data;
	domain->fwnode = info->fwnode;
	domain->hwirq_max = info->hwirq_max ?: info->size;
	domain->name = "lx_emul";
	if (info->init) {
		err = info->init(domain);
		if (err) {
			kfree(domain);
			return ERR_PTR(err);
		}
	}
	return domain;
}

void irq_dispose_mapping(unsigned int virq)
{
	struct lx_virq *v = lx_virq(virq);

	if (!v)
		return;
	if (v->domain->ops && v->domain->ops->unmap)
		v->domain->ops->unmap(v->domain, virq);
	memset(v, 0, sizeof(*v));
}

void irq_domain_remove(struct irq_domain *domain)
{
	unsigned int i;

	for (i = 0; i < LX_VIRQS; i++)
		if (lx_virqs[i].domain == domain)
			irq_dispose_mapping(LX_VIRQ_BASE + i);
	kfree(domain);
}

struct irq_desc *__irq_resolve_mapping(struct irq_domain *domain, irq_hw_number_t hwirq, unsigned int *irq)
{
	unsigned int i;

	for (i = 0; i < LX_VIRQS; i++) {
		if (lx_virqs[i].domain == domain && lx_virqs[i].hwirq == hwirq) {
			if (irq)
				*irq = LX_VIRQ_BASE + i;
			return &lx_virqs[i].desc;
		}
	}
	return NULL;
}

unsigned int irq_create_mapping_affinity(struct irq_domain *domain, irq_hw_number_t hwirq,
					 const struct irq_affinity_desc *affinity)
{
	unsigned int i, virq;

	if (__irq_resolve_mapping(domain, hwirq, &virq))
		return virq;
	for (i = 0; i < LX_VIRQS; i++) {
		struct lx_virq *v = &lx_virqs[i];

		if (v->domain)
			continue;
		virq = LX_VIRQ_BASE + i;
		v->domain = domain;
		v->hwirq = hwirq;
		v->desc.irq_data.irq = virq;
		v->desc.irq_data.hwirq = hwirq;
		v->desc.irq_data.domain = domain;
		v->desc.irq_data.common = &v->desc.irq_common_data;
		v->desc.irq_data.chip = &dummy_irq_chip;
		if (domain->ops && domain->ops->map && domain->ops->map(domain, virq, hwirq)) {
			memset(v, 0, sizeof(*v));
			return 0;
		}
		return virq;
	}
	pr_warn("lx_emul: out of virtual IRQ numbers\n");
	return 0;
}

struct irq_data *irq_get_irq_data(unsigned int irq)
{
	struct lx_virq *v = lx_virq(irq);

	return v ? &v->desc.irq_data : NULL;
}

void irq_set_chip_and_handler_name(unsigned int irq, const struct irq_chip *chip, irq_flow_handler_t handle,
				   const char *name)
{
	struct lx_virq *v = lx_virq(irq);

	if (!v)
		return;
	v->desc.irq_data.chip = (struct irq_chip *)chip;
	v->desc.handle_irq = handle;
	v->desc.name = name;
}

struct irq_chip dummy_irq_chip = {
	.name = "dummy",
};

/* The flow handler for demultiplexed sources: run what was requested on the number. */
void handle_simple_irq(struct irq_desc *desc)
{
	lx_emul_irq_handle(desc->irq_data.irq);
}

int generic_handle_domain_irq(struct irq_domain *domain, irq_hw_number_t hwirq)
{
	struct irq_desc *desc = __irq_resolve_mapping(domain, hwirq, NULL);

	if (!desc || !desc->handle_irq)
		return -EINVAL;
	desc->handle_irq(desc);
	return 0;
}

int generic_handle_irq_safe(unsigned int irq)
{
	struct lx_virq *v = lx_virq(irq);
	unsigned long flags;

	if (!v || !v->desc.handle_irq)
		return -EINVAL;
	local_irq_save(flags);
	v->desc.handle_irq(&v->desc);
	local_irq_restore(flags);
	return 0;
}

/* Nova's PCI devices have no firmware nodes, so no named interrupts either. */
int fwnode_irq_get_byname(const struct fwnode_handle *fwnode, const char *name)
{
	return -EINVAL;
}
