//! QEMU firmware configuration (fw_cfg): named files the host passes in with
//! `-fw_cfg name=opt/...,file=...`. Nova uses it for the host's Anthropic CLI login
//! (`opt/nova/claude-credential`, written by scripts/run.sh), so a short-lived token
//! never has to be typed or stored on the Nova disk.
//!
//! Legacy I/O interface: selector port 0x510 (16-bit), data port 0x511 (8-bit). Present
//! on QEMU's pc and q35 machines; on other hardware the signature check fails.

use alloc::vec::Vec;

use crate::port::{inb, outw};

const SELECTOR: u16 = 0x510;
const DATA: u16 = 0x511;
const SIGNATURE: u16 = 0x0000;
const FILE_DIR: u16 = 0x0019;

fn select(key: u16) {
    // SAFETY: the fw_cfg selector port; only used after the signature check (or for it).
    unsafe { outw(SELECTOR, key) }
}

fn read(buf: &mut [u8]) {
    for b in buf {
        // SAFETY: the fw_cfg data port; reads advance through the selected item.
        *b = unsafe { inb(DATA) };
    }
}

fn present() -> bool {
    select(SIGNATURE);
    let mut sig = [0u8; 4];
    read(&mut sig);
    &sig == b"QEMU"
}

/// Contents of the fw_cfg file `name`, if the host provided one.
pub fn read_file(name: &str) -> Option<Vec<u8>> {
    if !present() {
        return None;
    }
    select(FILE_DIR);
    let mut count = [0u8; 4];
    read(&mut count);
    for _ in 0..u32::from_be_bytes(count) {
        // struct FWCfgFile { be32 size; be16 select; u16 reserved; char name[56]; }
        let mut entry = [0u8; 64];
        read(&mut entry);
        let size = u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]) as usize;
        let key = u16::from_be_bytes([entry[4], entry[5]]);
        let raw = &entry[8..];
        let entry_name = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
        if entry_name == name.as_bytes() {
            if size > 64 * 1024 {
                return None;
            }
            select(key);
            let mut data = alloc::vec![0u8; size];
            read(&mut data);
            return Some(data);
        }
    }
    None
}
