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
 * putuser.S): get takes the address in %rax and returns the value in %rdx and 0 (or
 * -EFAULT) in %rax; put takes the value in %rax and the address in %rcx and returns 0
 * (or -EFAULT) in %rcx. Every other register is preserved. The address is checked
 * against the program's mappings first (nova_lx_user_ok, user.c; kernel pointers pass);
 * a page that is not present yet is filled by Nova's page-fault handler.
 */
.macro LX_USER_CHECK addr, size, write
	push	%rax
	push	%rcx
	push	%rdx
	push	%rsi
	push	%rdi
	push	%r8
	push	%r9
	push	%r10
	push	%r11
	mov	\addr, %rdi
	mov	$\size, %esi
	mov	$\write, %edx
	call	nova_lx_user_ok
	test	%al, %al
	pop	%r11
	pop	%r10
	pop	%r9
	pop	%r8
	pop	%rdi
	pop	%rsi
	pop	%rdx
	pop	%rcx
	pop	%rax
.endm

.macro LX_GET_USER size, load
SYM_FUNC_START(__get_user_\size)
	LX_USER_CHECK %rax, \size, 0
	jz	1f
	\load
	xorl	%eax, %eax
	RET
1:	xorl	%edx, %edx
	movq	$-14, %rax		/* -EFAULT */
	RET
SYM_FUNC_END(__get_user_\size)
.endm

.macro LX_PUT_USER size, store
SYM_FUNC_START(__put_user_\size)
	LX_USER_CHECK %rcx, \size, 1
	jz	1f
	\store
	xorl	%ecx, %ecx
	RET
1:	movq	$-14, %rcx		/* -EFAULT */
	RET
SYM_FUNC_END(__put_user_\size)
.endm

LX_GET_USER 1, "movzbl (%rax), %edx"
LX_GET_USER 2, "movzwl (%rax), %edx"
LX_GET_USER 4, "movl (%rax), %edx"
LX_GET_USER 8, "movq (%rax), %rdx"
LX_PUT_USER 1, "movb %al, (%rcx)"
LX_PUT_USER 2, "movw %ax, (%rcx)"
LX_PUT_USER 4, "movl %eax, (%rcx)"
LX_PUT_USER 8, "movq %rax, (%rcx)"
