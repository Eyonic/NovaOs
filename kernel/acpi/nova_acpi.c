// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * Nova's use of uACPI (kernel/third_party/uacpi): bring up ACPI in two stages, hand
 * Nova the static tables it needs (MADT, MCFG), and answer PCI interrupt routing
 * questions from the firmware's AML (_PRT and interrupt link devices). uACPI's kernel
 * API is implemented in Rust (kernel/src/acpi.rs).
 */
#include <uacpi/uacpi.h>
#include <uacpi/tables.h>
#include <uacpi/utilities.h>
#include <uacpi/resources.h>
#include <uacpi/event.h>
#include <uacpi/sleep.h>
#include <uacpi/acpi.h>

/* Stage 1: the table subsystem only (no AML), so Nova can read the MADT and MCFG
 * and set up its interrupt controllers before any ACPI interrupt can fire. */
int nova_acpi_tables_init(void)
{
	return uacpi_initialize(0);
}

/* A table by signature ("APIC", "MCFG", ...): its virtual address and length. The
 * reference is kept: tables stay mapped for the life of the system. */
int nova_acpi_table(const char *signature, const void **out, uacpi_u32 *length)
{
	uacpi_table table;
	uacpi_status st = uacpi_table_find_by_signature(signature, &table);

	if (st != UACPI_STATUS_OK)
		return st;
	*out = table.ptr;
	*length = table.hdr->length;
	return UACPI_STATUS_OK;
}

/* Stage 2: load and initialize the AML namespace in the given interrupt model
 * (0 = PIC, 1 = I/O APIC; firmware routes PCI interrupts differently for each). */
int nova_acpi_namespace_init(int ioapic)
{
	uacpi_status st;

	st = uacpi_namespace_load();
	if (st != UACPI_STATUS_OK)
		return st;
	st = uacpi_set_interrupt_model(ioapic ? UACPI_INTERRUPT_MODEL_IOAPIC : UACPI_INTERRUPT_MODEL_PIC);
	if (st != UACPI_STATUS_OK)
		return st;
	st = uacpi_namespace_initialize();
	if (st != UACPI_STATUS_OK)
		return st;
	return uacpi_finalize_gpe_initialization();
}

/* ---------------------------------------------------------------- PCI routing */

static uacpi_iteration_decision find_first(void *user, uacpi_namespace_node *node, uacpi_u32 depth)
{
	*(uacpi_namespace_node **)user = node;
	return UACPI_ITERATION_DECISION_BREAK;
}

static uacpi_namespace_node *pci_root(void)
{
	static uacpi_namespace_node *root;
	static const uacpi_char *const hids[] = { "PNP0A08", "PNP0A03", UACPI_NULL };

	if (!root)
		uacpi_find_devices_at(uacpi_namespace_get_predefined(UACPI_PREDEFINED_NAMESPACE_SB), hids,
				      find_first, &root);
	return root;
}

struct link_irq {
	uacpi_u32 index, gsi;
	int level, active_low, found;
};

static uacpi_iteration_decision link_resource(void *user, uacpi_resource *res)
{
	struct link_irq *out = user;

	if (res->type == UACPI_RESOURCE_TYPE_IRQ && res->irq.num_irqs > out->index) {
		out->gsi = res->irq.irqs[out->index];
		out->level = res->irq.triggering == UACPI_TRIGGERING_LEVEL;
		out->active_low = res->irq.polarity == UACPI_POLARITY_ACTIVE_LOW;
		out->found = 1;
		return UACPI_ITERATION_DECISION_BREAK;
	}
	if (res->type == UACPI_RESOURCE_TYPE_EXTENDED_IRQ && res->extended_irq.num_irqs > out->index) {
		out->gsi = res->extended_irq.irqs[out->index];
		out->level = res->extended_irq.triggering == UACPI_TRIGGERING_LEVEL;
		out->active_low = res->extended_irq.polarity == UACPI_POLARITY_ACTIVE_LOW;
		out->found = 1;
		return UACPI_ITERATION_DECISION_BREAK;
	}
	return UACPI_ITERATION_DECISION_CONTINUE;
}

/*
 * The global system interrupt of INTx pin `pin` (0 = INTA) of device `slot` on the root
 * bus, from the root bridge's _PRT. Returns the GSI, or -1 if the firmware has no route.
 * `flags` gets bit 0 = level triggered, bit 1 = active low. (Devices behind bridges are
 * routed through the bridge's own _PRT or INTx swizzling; Nova uses MSI for those.)
 */
int nova_acpi_pci_irq(uacpi_u8 slot, uacpi_u8 pin, uacpi_u32 *flags)
{
	uacpi_namespace_node *root = pci_root();
	uacpi_pci_routing_table *prt;
	int gsi = -1;

	if (!root || uacpi_get_pci_routing_table(root, &prt) != UACPI_STATUS_OK)
		return -1;
	for (uacpi_size i = 0; i < prt->num_entries; i++) {
		uacpi_pci_routing_table_entry *e = &prt->entries[i];

		if ((e->address >> 16) != slot || e->pin != pin)
			continue;
		if (!e->source) {
			/* A hard-wired GSI: PCI interrupts are level triggered, active low. */
			gsi = e->index;
			*flags = 3;
		} else {
			struct link_irq link = { .index = e->index };
			uacpi_resources *res;

			if (uacpi_get_current_resources(e->source, &res) == UACPI_STATUS_OK) {
				uacpi_for_each_resource(res, link_resource, &link);
				uacpi_free_resources(res);
			}
			if (link.found) {
				gsi = link.gsi;
				*flags = (link.level ? 1 : 0) | (link.active_low ? 2 : 0);
			}
		}
		break;
	}
	uacpi_free_pci_routing_table(prt);
	return gsi;
}

/* ---------------------------------------------------------------- power */

/* ACPI S5 (soft off). Returns only on failure. */
int nova_acpi_power_off(void)
{
	uacpi_status st = uacpi_prepare_for_sleep_state(UACPI_SLEEP_STATE_S5);

	if (st != UACPI_STATUS_OK)
		return st;
	__asm__ volatile("cli");
	return uacpi_enter_sleep_state(UACPI_SLEEP_STATE_S5);
}

/* ACPI reset register (FADT), the modern way to reboot. Returns only on failure. */
int nova_acpi_reboot(void)
{
	return uacpi_reboot();
}
