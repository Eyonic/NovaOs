// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul kernel FPU sections: kernel_fpu_begin() / kernel_fpu_end().
 *
 * Nova's own kernel code never touches the FPU, SSE or AVX registers (its Rust target is
 * soft-float, uACPI is built with -mgeneral-regs-only), so their live contents always
 * belong to a user program: the one running, or the last one that ran (process.rs saves
 * and restores a program's state when its task is switched out and in). Linux code that
 * uses floating point (amdgpu's display core computes bandwidth and clocks with it, DRM
 * copies from write-combined memory with SSE4.1) brackets it with these functions.
 *
 * kernel_fpu_begin() saves whatever is live (XSAVE of every enabled component, or FXSAVE
 * without XSAVE) and gives the caller clean x87 and MXCSR state; kernel_fpu_end() puts
 * the saved state back, so the user program never notices. Sections may nest (an
 * interrupt handler using the FPU inside a task's section): each level has its own save
 * area. Like Linux, a section must not sleep; nothing in Nova switches tasks inside one
 * unless the code calls schedule() or a contended lock, which Linux forbids as well.
 */
#include <linux/preempt.h>
#include <linux/irqflags.h>
#include <linux/bug.h>
#include <linux/printk.h>
#include <asm/fpu/api.h>
#include "nova.h"

#define LX_FPU_LEVELS 4
#define LX_FPU_AREA 4096	/* XSAVE with AVX-512 needs 2696 bytes */
#define LX_MXCSR_DEFAULT 0x1f80

static u8 lx_fpu_area[LX_FPU_LEVELS][LX_FPU_AREA] __aligned(64);
static unsigned int lx_fpu_depth;
static bool lx_fpu_xsave, lx_fpu_ready;

static void lx_fpu_init(void)
{
	u64 size = nova_fpu_state_size();

	/* An XSAVE area larger than ours cannot happen with the components Nova enables
	 * (x87, SSE, AVX, AVX-512). Should it, FXSAVE still covers what Linux's FPU code
	 * touches: it is built for SSE2 only, and legacy SSE instructions leave the upper
	 * halves of the AVX registers alone. */
	lx_fpu_xsave = size != 0 && size <= LX_FPU_AREA;
	if (size > LX_FPU_AREA)
		pr_warn("lx_emul: XSAVE area of %llu bytes; kernel FPU sections save x87/SSE only\n", size);
	lx_fpu_ready = true;
}

void kernel_fpu_begin_mask(unsigned int kfpu_mask)
{
	unsigned long flags;
	u8 *area;

	preempt_disable();
	local_irq_save(flags);
	if (!lx_fpu_ready)
		lx_fpu_init();
	if (WARN_ON_ONCE(lx_fpu_depth >= LX_FPU_LEVELS)) {
		/* Keep the count balanced for kernel_fpu_end; this level is not saved. */
		lx_fpu_depth++;
		local_irq_restore(flags);
		return;
	}
	area = lx_fpu_area[lx_fpu_depth++];
	if (lx_fpu_xsave)
		asm volatile("xsave64 (%0)" : : "r"(area), "a"(-1), "d"(-1) : "memory");
	else
		asm volatile("fxsave64 (%0)" : : "r"(area) : "memory");
	local_irq_restore(flags);

	if (kfpu_mask & KFPU_387)
		asm volatile("fninit");
	if (kfpu_mask & KFPU_MXCSR) {
		u32 mxcsr = LX_MXCSR_DEFAULT;

		asm volatile("ldmxcsr %0" : : "m"(mxcsr));
	}
}

void kernel_fpu_end(void)
{
	unsigned long flags;
	u8 *area;

	local_irq_save(flags);
	if (WARN_ON_ONCE(!lx_fpu_depth)) {
		local_irq_restore(flags);
		return;
	}
	if (--lx_fpu_depth < LX_FPU_LEVELS) {
		area = lx_fpu_area[lx_fpu_depth];
		if (lx_fpu_xsave)
			asm volatile("xrstor64 (%0)" : : "r"(area), "a"(-1), "d"(-1) : "memory");
		else
			asm volatile("fxrstor64 (%0)" : : "r"(area) : "memory");
	}
	local_irq_restore(flags);
	preempt_enable();
}
