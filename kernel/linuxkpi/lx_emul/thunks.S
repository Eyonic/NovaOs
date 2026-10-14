/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * preempt_enable() calls this when the preempt count drops to zero with a reschedule
 * pending. lx_emul never marks one pending (base.c), and Nova's kernel tasks switch
 * cooperatively, so there is nothing to do; like Linux's thunk it preserves every register.
 */
#include <linux/linkage.h>

SYM_FUNC_START(preempt_schedule_thunk)
	RET
SYM_FUNC_END(preempt_schedule_thunk)
