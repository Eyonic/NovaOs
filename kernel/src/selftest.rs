//! Boot-time self-tests selected with the kernel command line `nova.test=<case>`.
//!
//! Each fault case is a tiny assembly routine with a global label on the exact
//! instruction expected in the exception report's RIP. The kernel prints that
//! address before triggering, and scripts/test.sh checks the report against it.
//! A wrong frame layout (e.g. mixing up error-code and no-error-code vectors)
//! shifts every field and fails this check.

use core::sync::atomic::Ordering;

use crate::{interrupts, println};

core::arch::global_asm!(
    r#"
    .section .text.nova_selftest, "ax"

    .global nova_selftest_de
    nova_selftest_de:
        xor eax, eax
        xor edx, edx
        xor ecx, ecx
    .global nova_selftest_de_rip
    nova_selftest_de_rip:
        div ecx                         // #DE: divide by zero (fault: RIP = this instruction)
        ret

    .global nova_selftest_ud
    nova_selftest_ud:
    .global nova_selftest_ud_rip
    nova_selftest_ud_rip:
        ud2                             // #UD
        ret

    .global nova_selftest_gp
    nova_selftest_gp:
        mov eax, 0x28                   // selector index 5: beyond the 5-entry GDT limit
    .global nova_selftest_gp_rip
    nova_selftest_gp_rip:
        mov ds, ax                      // #GP(0x28)
        ret

    .global nova_selftest_pf
    nova_selftest_pf:
        movabs rax, 0xdeadbeef000       // lower half: unmapped under Limine base revision >= 1
    .global nova_selftest_pf_rip
    nova_selftest_pf_rip:
        mov byte ptr [rax], 0x42        // #PF, error code 0x2 (not-present, write, supervisor)
        ret

    .global nova_selftest_bp
    nova_selftest_bp:
        int3                            // #BP is a trap: saved RIP is the *next* instruction
    .global nova_selftest_bp_rip
    nova_selftest_bp_rip:
        ret

    .global nova_selftest_df
    nova_selftest_df:
        movabs rax, 0xdeadbeef000
        mov rsp, rax                    // unusable stack
        push rax                        // #PF; delivering it needs the same bad stack -> #PF -> #DF (IST1)
        ret
    "#
);

unsafe extern "C" {
    fn nova_selftest_de();
    fn nova_selftest_ud();
    fn nova_selftest_gp();
    fn nova_selftest_pf();
    fn nova_selftest_bp();
    fn nova_selftest_df();
    static nova_selftest_de_rip: u8;
    static nova_selftest_ud_rip: u8;
    static nova_selftest_gp_rip: u8;
    static nova_selftest_pf_rip: u8;
    static nova_selftest_bp_rip: u8;
}

/// Run the self-test named by `nova.test=` in `cmdline`, if any. Fault cases do not return.
pub fn run(cmdline: &str) {
    let Some(case) = cmdline.split_ascii_whitespace().find_map(|arg| arg.strip_prefix("nova.test=")) else {
        return;
    };
    println!("[nova] selftest: {case}");

    // SAFETY (all arms): each routine deliberately raises the named exception, whose
    // handler reports and halts; none of them return. Taking a label's address reads nothing.
    unsafe {
        match case {
            "de" => trigger("#DE", nova_selftest_de, &raw const nova_selftest_de_rip),
            "ud" => trigger("#UD", nova_selftest_ud, &raw const nova_selftest_ud_rip),
            "gp" => trigger("#GP", nova_selftest_gp, &raw const nova_selftest_gp_rip),
            "pf" => trigger("#PF", nova_selftest_pf, &raw const nova_selftest_pf_rip),
            "bp" => trigger("#BP", nova_selftest_bp, &raw const nova_selftest_bp_rip),
            "df" => {
                println!("[nova] selftest: triggering #DF via stack fault (saved RIP undefined)");
                nova_selftest_df();
            }
            "nested" => {
                interrupts::FAULT_INSIDE_HANDLER.store(true, Ordering::SeqCst);
                println!("[nova] selftest: triggering #BP with a #UD inside the reporter");
                nova_selftest_bp();
            }
            "panic" => panic!("intentional panic requested by nova.test=panic"),
            other => panic!("unknown self-test nova.test={other}"),
        }
    }
    panic!("self-test {case} returned; the expected exception did not happen");
}

unsafe fn trigger(name: &str, routine: unsafe extern "C" fn(), expected_rip: *const u8) {
    println!("[nova] selftest: triggering {name}, expected RIP {:#018x}", expected_rip as u64);
    // SAFETY: see `run`.
    unsafe { routine() };
}
