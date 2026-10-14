//! Pointing device: PS/2 mouse on the 8042's second port (IRQ 12), upgraded to
//! absolute positioning through the VMware "vmmouse" backdoor when the hypervisor
//! offers it (QEMU does by default), so the pointer tracks the host cursor without
//! grabbing it.
//!
//! References: OSDev wiki "PS/2 Mouse"; Linux drivers/input/mouse/vmmouse.c for the
//! backdoor protocol (port 0x5658, magic 0x564D5868).

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::input::Ring;
use crate::port::{inb, outb};
use crate::sync::SpinLock;

/// Raw bytes from IRQ 12.
pub static BYTES: Ring = Ring::new();

static ABSOLUTE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
pub enum Motion {
    /// Relative movement (PS/2).
    Relative(i32, i32),
    /// Absolute position, 0..=0xFFFF on both axes (vmmouse).
    Absolute(u32, u32),
}

#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub motion: Motion,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    /// Wheel steps (positive = towards the user).
    pub wheel: i32,
}

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;

fn wait_write() -> bool {
    // SAFETY (all port access here): 8042 controller ports.
    (0..100_000).any(|_| unsafe { inb(STATUS) } & 2 == 0)
}

fn wait_read() -> Option<u8> {
    (0..100_000).any(|_| unsafe { inb(STATUS) } & 1 != 0).then(|| unsafe { inb(DATA) })
}

fn controller(cmd: u8) -> bool {
    wait_write() && {
        unsafe { outb(STATUS, cmd) };
        true
    }
}

/// Send a byte to the mouse and expect ACK (0xFA).
fn mouse_cmd(byte: u8) -> bool {
    controller(0xD4) && wait_write() && {
        unsafe { outb(DATA, byte) };
        wait_read() == Some(0xFA)
    }
}

/// Enable the second PS/2 port and the mouse. Interrupts must be disabled.
pub fn init() -> bool {
    if !controller(0xA8) {
        return false;
    }
    if !controller(0x20) {
        return false;
    }
    let Some(config) = wait_read() else { return false };
    // bit 1: port 2 IRQ on; bit 5: port 2 clock on (0).
    let config = (config | 0x02) & !0x20;
    if !controller(0x60) || !wait_write() {
        return false;
    }
    unsafe { outb(DATA, config) };
    if !(mouse_cmd(0xF6) && mouse_cmd(0xF4)) {
        return false;
    }
    if vmmouse_enable() {
        ABSOLUTE.store(true, Ordering::Relaxed);
    }
    true
}

pub fn absolute() -> bool {
    ABSOLUTE.load(Ordering::Relaxed)
}

const VMWARE_MAGIC: u32 = 0x564D_5868;
const VMWARE_PORT: u16 = 0x5658;
const CMD_GETVERSION: u32 = 10;
const CMD_ABSPOINTER_DATA: u32 = 39;
const CMD_ABSPOINTER_STATUS: u32 = 40;
const CMD_ABSPOINTER_COMMAND: u32 = 41;
const ABSPOINTER_ENABLE: u32 = 0x4541_4552;
const ABSPOINTER_RELATIVE: u32 = 0xF5;
const ABSPOINTER_ABSOLUTE: u32 = 0x5342_4152;
const VMMOUSE_VERSION_ID: u32 = 0x3442_554A;

/// One backdoor call: returns (eax, ebx, ecx, edx).
fn backdoor(cmd: u32, arg: u32) -> (u32, u32, u32, u32) {
    let (a, c, d): (u32, u32, u32);
    let mut b = arg as u64;
    // SAFETY: the VMware backdoor is an `in` from port 0x5658 with a magic value;
    // on hardware without it the read returns garbage, which callers validate.
    // RBX is reserved by LLVM, so it is swapped through a scratch register.
    unsafe {
        asm!(
            "xchg {b}, rbx",
            "in eax, dx",
            "xchg {b}, rbx",
            b = inout(reg) b,
            inout("eax") VMWARE_MAGIC => a,
            inout("ecx") cmd => c,
            inout("edx") VMWARE_PORT as u32 => d,
            options(nostack),
        );
    }
    (a, b as u32, c, d)
}

fn vmmouse_enable() -> bool {
    let (_, magic, _, _) = backdoor(CMD_GETVERSION, !VMWARE_MAGIC);
    if magic != VMWARE_MAGIC {
        return false;
    }
    backdoor(CMD_ABSPOINTER_COMMAND, ABSPOINTER_ENABLE);
    let (status, ..) = backdoor(CMD_ABSPOINTER_STATUS, 0);
    if status & 0xFFFF == 0 || status == 0xFFFF_0000 {
        return false;
    }
    let (id, ..) = backdoor(CMD_ABSPOINTER_DATA, 1);
    if id != VMMOUSE_VERSION_ID {
        return false;
    }
    backdoor(CMD_ABSPOINTER_COMMAND, ABSPOINTER_ABSOLUTE);
    true
}

/// Return to relative mode (e.g. before handing the machine back).
#[allow(dead_code)]
pub fn vmmouse_disable() {
    backdoor(CMD_ABSPOINTER_COMMAND, ABSPOINTER_RELATIVE);
}

struct Decoder {
    packet: [u8; 3],
    len: usize,
}

static DECODER: SpinLock<Decoder> = SpinLock::new(Decoder { packet: [0; 3], len: 0 });

/// Next pointer report, if any.
pub fn poll() -> Option<Report> {
    let mut dec = DECODER.try_lock()?;
    if absolute() {
        // IRQ 12 only signals that the backdoor has data; the PS/2 bytes are dummies.
        while BYTES.pop().is_some() {}
        let (status, ..) = backdoor(CMD_ABSPOINTER_STATUS, 0);
        if status == 0xFFFF_0000 {
            // Error: re-enable.
            vmmouse_enable();
            return None;
        }
        if status & 0xFFFF < 4 {
            return None;
        }
        let (buttons, x, y, z) = backdoor(CMD_ABSPOINTER_DATA, 4);
        return Some(Report {
            motion: Motion::Absolute(x & 0xFFFF, y & 0xFFFF),
            left: buttons & 0x20 != 0,
            right: buttons & 0x10 != 0,
            middle: buttons & 0x08 != 0,
            wheel: z as i8 as i32,
        });
    }
    while let Some(byte) = BYTES.pop() {
        if dec.len == 0 && byte & 0x08 == 0 {
            continue; // resynchronise: bit 3 is always set in the first byte
        }
        let i = dec.len;
        dec.packet[i] = byte;
        dec.len += 1;
        if dec.len == 3 {
            dec.len = 0;
            let [b, x, y] = dec.packet;
            if b & 0xC0 != 0 {
                continue; // overflow
            }
            let dx = x as i32 - if b & 0x10 != 0 { 256 } else { 0 };
            let dy = y as i32 - if b & 0x20 != 0 { 256 } else { 0 };
            return Some(Report { motion: Motion::Relative(dx, -dy), left: b & 1 != 0, right: b & 2 != 0, middle: b & 4 != 0, wheel: 0 });
        }
    }
    None
}
