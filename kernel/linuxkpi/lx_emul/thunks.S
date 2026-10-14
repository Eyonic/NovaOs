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

/*
 * __get_user_N / __put_user_N with Linux's register convention (arch/x86/lib/getuser.S,
 * putuser.S): get takes the address in %rax and returns the value in %rdx and 0 in
 * %rax; put takes the value in %rax and the address in %rcx and returns 0 in %rcx. All
 * Linux code in Nova runs in the kernel with kernel pointers, so these are plain loads
 * and stores (no fault handling needed).
 */
SYM_FUNC_START(__get_user_1)
	movzbl	(%rax), %edx
	xorl	%eax, %eax
	RET
SYM_FUNC_END(__get_user_1)

SYM_FUNC_START(__get_user_2)
	movzwl	(%rax), %edx
	xorl	%eax, %eax
	RET
SYM_FUNC_END(__get_user_2)

SYM_FUNC_START(__get_user_4)
	movl	(%rax), %edx
	xorl	%eax, %eax
	RET
SYM_FUNC_END(__get_user_4)

SYM_FUNC_START(__get_user_8)
	movq	(%rax), %rdx
	xorl	%eax, %eax
	RET
SYM_FUNC_END(__get_user_8)

SYM_FUNC_START(__put_user_1)
	movb	%al, (%rcx)
	xorl	%ecx, %ecx
	RET
SYM_FUNC_END(__put_user_1)

SYM_FUNC_START(__put_user_2)
	movw	%ax, (%rcx)
	xorl	%ecx, %ecx
	RET
SYM_FUNC_END(__put_user_2)

SYM_FUNC_START(__put_user_4)
	movl	%eax, (%rcx)
	xorl	%ecx, %ecx
	RET
SYM_FUNC_END(__put_user_4)

SYM_FUNC_START(__put_user_8)
	movq	%rax, (%rcx)
	xorl	%ecx, %ecx
	RET
SYM_FUNC_END(__put_user_8)
