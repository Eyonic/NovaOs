//! Intel High Definition Audio: controller, codecs, and one 48 kHz stereo output stream.
//!
//! One driver covers every HD Audio controller (Intel ICH/PCH, AMD FCH and the HDMI audio
//! functions of graphics cards): the register interface is a single standard. At probe
//! the controller is reset, commands to the codecs go through the CORB/RIRB rings (or the
//! immediate command registers if the rings do not answer), and every codec's widget
//! graph is walked for output pins with a path to a DAC. [`start`] then picks one codec
//! (analog before HDMI), powers and unmutes every output path on it (line out, headphone
//! and speaker together, all fed by the same stream, so whichever jack is used plays), and
//! sets up an output stream descriptor.
//!
//! The stream is polled like Nova's other drivers: its buffer descriptor list covers one
//! 64 KiB cyclic DMA buffer (about 340 ms) that [`write`] fills ahead of the controller's
//! position (LPIB), and a kernel task zeroes what has been played so an underrun plays
//! silence instead of old samples. The stream runs while there is something to play and
//! stops after a second of silence.
//!
//! Vendor details: AMD (and ATI southbridge) controllers snoop DMA only when told to
//! (PCI config 0x42 bits 0-2), Intel's TCSEL (0x44) is cleared to traffic class 0, and
//! the CPU writes back its caches over every buffer it hands the controller because the
//! HDMI audio functions of AMD graphics cards do not snoop. HDMI outputs get an audio
//! infoframe for two channels (and on AMD codecs the channel allocation verb instead).
//!
//! Reference: Intel High Definition Audio Specification rev. 1.0a (sections 3.3
//! registers, 3.6 buffer descriptor list, 4.4 command/response rings, 7.3 verbs and
//! parameters, 7.3.3.31 configuration default).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::memory::{self, DmaBuffer};
use crate::pci::{Bar, Device};
use crate::sync::SpinLock;
use crate::{pit, task};

// Controller registers.
const GCAP: usize = 0x00;
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0E;
const INTCTL: usize = 0x20;
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4A;
const CORBCTL: usize = 0x4C;
const CORBSIZE: usize = 0x4E;
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5A;
const RIRBCTL: usize = 0x5C;
const RIRBSTS: usize = 0x5D;
const RIRBSIZE: usize = 0x5E;
const ICOI: usize = 0x60;
const ICII: usize = 0x64;
const ICIS: usize = 0x68;
/// Stream descriptors: input streams first, then output, then bidirectional.
const SD_BASE: usize = 0x80;
const SD_CTL: usize = 0x00;
const SD_STS: usize = 0x03;
const SD_LPIB: usize = 0x04;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0C;
const SD_FIFOS: usize = 0x10;
const SD_FMT: usize = 0x12;
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1C;

const GCTL_CRST: u32 = 1 << 0;
const CORBRP_RST: u16 = 1 << 15;
const RIRBWP_RST: u16 = 1 << 15;
const RING_RUN: u8 = 1 << 1; // CORBCTL.CORBRUN, RIRBCTL.RIRBDMAEN
const ICIS_BUSY: u16 = 1 << 0;
const ICIS_VALID: u16 = 1 << 1;
const SD_CTL_SRST: u8 = 1 << 0;
const SD_CTL_RUN: u8 = 1 << 1;

// Verbs (12-bit identifier with an 8-bit payload, or 4-bit with 16).
const GET_PARAM: u32 = 0xF0000;
const GET_CONN_LIST: u32 = 0xF0200;
const SET_CONN_SELECT: u32 = 0x70100;
const SET_POWER: u32 = 0x70500;
const SET_STREAM: u32 = 0x70600;
const SET_PIN_CTL: u32 = 0x70700;
const GET_PIN_SENSE: u32 = 0xF0900;
const SET_EAPD: u32 = 0x70C00;
const SET_DIGI_CONV: u32 = 0x70D00;
const GET_CONFIG_DEFAULT: u32 = 0xF1C00;
const SET_CONV_CHANNELS: u32 = 0x72D00;
const SET_DIP_INDEX: u32 = 0x73000;
const SET_DIP_DATA: u32 = 0x73100;
const SET_DIP_XMIT: u32 = 0x73200;
/// AMD/ATI HDMI codecs: speaker allocation for the infoframe the codec sends itself.
const ATI_SET_CHANNEL_ALLOCATION: u32 = 0x77100;
const SET_FORMAT: u32 = 0x20000;
const SET_AMP: u32 = 0x30000;

// Parameters (GET_PARAM).
const P_VENDOR: u32 = 0x00;
const P_NODES: u32 = 0x04;
const P_FUNCTION_TYPE: u32 = 0x05;
const P_WIDGET_CAPS: u32 = 0x09;
const P_PIN_CAPS: u32 = 0x0C;
const P_IN_AMP: u32 = 0x0D;
const P_CONN_LEN: u32 = 0x0E;
const P_OUT_AMP: u32 = 0x12;

// Widget types (widget capabilities bits 23:20).
const W_DAC: u8 = 0;
const W_MIXER: u8 = 2;
const W_SELECTOR: u8 = 3;
const W_PIN: u8 = 4;

const WCAP_IN_AMP: u32 = 1 << 1;
const WCAP_OUT_AMP: u32 = 1 << 2;
const WCAP_AMP_OVERRIDE: u32 = 1 << 3;
const WCAP_DIGITAL: u32 = 1 << 9;
const WCAP_POWER: u32 = 1 << 10;
const PINCAP_PRESENCE: u32 = 1 << 2;
const PINCAP_HP_DRIVE: u32 = 1 << 3;
const PINCAP_OUT: u32 = 1 << 4;
const PINCAP_HDMI: u32 = 1 << 7;
const PINCAP_EAPD: u32 = 1 << 16;
const PINCAP_DP: u32 = 1 << 24;

pub const RATE: u32 = 48000;
pub const CHANNELS: usize = 2;
const FRAME: usize = 4;
/// Stream format: PCM, 48 kHz base, 16 bits, 2 channels.
const FORMAT_48K_16_STEREO: u16 = 0x0011;
const STREAM_TAG: u8 = 1;
const BDL_ENTRIES: usize = 16;
const CHUNK: usize = 4096;
const BUF_SIZE: usize = BDL_ENTRIES * CHUNK;
/// Bytes behind the reported position that are left alone: some controllers (AMD
/// among them) report LPIB up to a FIFO's worth ahead of what they have fetched.
const LAG: u64 = 2048;
/// After an underrun, new samples start this far ahead of the position, past the FIFO.
const LEAD: u64 = 1024;
/// The stream stops after this many ticks with nothing to play.
const IDLE_STOP: u64 = pit::HZ;

fn read8(regs: u64, reg: usize) -> u8 {
    // SAFETY: `regs` maps the controller's register BAR; reg is a register offset inside it.
    unsafe { ((regs as usize + reg) as *const u8).read_volatile() }
}
fn read16(regs: u64, reg: usize) -> u16 {
    // SAFETY: as for read8.
    unsafe { ((regs as usize + reg) as *const u16).read_volatile() }
}
fn read32(regs: u64, reg: usize) -> u32 {
    // SAFETY: as for read8.
    unsafe { ((regs as usize + reg) as *const u32).read_volatile() }
}
fn write8(regs: u64, reg: usize, v: u8) {
    // SAFETY: as for read8.
    unsafe { ((regs as usize + reg) as *mut u8).write_volatile(v) }
}
fn write16(regs: u64, reg: usize, v: u16) {
    // SAFETY: as for read8.
    unsafe { ((regs as usize + reg) as *mut u16).write_volatile(v) }
}
fn write32(regs: u64, reg: usize, v: u32) {
    // SAFETY: as for read8.
    unsafe { ((regs as usize + reg) as *mut u32).write_volatile(v) }
}

/// Write back (and invalidate) the cache lines over `len` bytes at `ptr`, so a device that
/// does not snoop sees what the CPU wrote and the CPU then reads what the device wrote.
fn flush_cache(ptr: *const u8, len: usize) {
    let start = ptr as usize & !63;
    let end = ptr as usize + len;
    let mut line = start;
    while line < end {
        // SAFETY: clflush on addresses of an owned buffer; it only affects caching.
        unsafe { core::arch::asm!("clflush [{}]", in(reg) line, options(nostack, preserves_flags)) };
        line += 64;
    }
    // SAFETY: a fence orders the flushes before the device is told about the data.
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
}

/// Wait (yielding to other tasks) until `done()` or `ticks` timer ticks pass.
fn wait_until(ticks: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = pit::ticks() + ticks.max(2);
    let mut spins = 0u32;
    loop {
        if done() {
            return true;
        }
        if pit::ticks() >= end {
            return false;
        }
        spins += 1;
        if spins > 1000 {
            task::yield_now();
        }
        core::hint::spin_loop();
    }
}

fn delay_us(us: u64) {
    let end = pit::nanos() + us * 1000;
    while pit::nanos() < end {
        core::hint::spin_loop();
    }
}

/// A controller and its command transport.
struct Controller {
    regs: u64,
    corb: DmaBuffer,
    rirb: DmaBuffer,
    corb_entries: u16,
    rirb_entries: u16,
    rirb_rp: u16,
    /// Commands go through the immediate command registers (the rings did not answer).
    immediate: bool,
    /// Register offset of the first output stream descriptor.
    out_sd: usize,
    dma64: bool,
}

impl Controller {
    fn new(dev: &Device) -> Result<Self, String> {
        let Some(Bar::Memory { base, size }) = dev.bar(0) else { return Err("BAR0 is not a memory BAR".into()) };
        dev.enable();
        let a = dev.address;
        match dev.vendor {
            // AMD FCH and ATI southbridge controllers: enable snooping (Misc Control 2, bits 0-2 = 2).
            0x1022 => a.write8(0x42, a.read8(0x42) & !0x07 | 0x02),
            0x1002 if matches!(dev.device, 0x437B | 0x4383) => a.write8(0x42, a.read8(0x42) & !0x07 | 0x02),
            // Intel: traffic class 0 (TCSEL), which some codecs need to play without static.
            0x8086 => a.write8(0x44, a.read8(0x44) & !0x07),
            _ => {}
        }
        let regs = memory::map_mmio(base, size.max(0x4000));
        let gcap = read16(regs, GCAP);
        let (iss, oss) = ((gcap >> 8) & 0xF, (gcap >> 12) & 0xF);
        if oss == 0 {
            return Err("no output streams".into());
        }
        // Controllers without 64-bit addressing (GCAP.64OK) need their rings and buffers
        // below 4 GiB, which the heap does not promise.
        let dma64 = gcap & 1 != 0;
        let mut c = Self {
            regs,
            dma64,
            corb: DmaBuffer::new(1024, 128),
            rirb: DmaBuffer::new(2048, 128),
            corb_entries: 256,
            rirb_entries: 256,
            rirb_rp: 0,
            immediate: false,
            out_sd: SD_BASE + iss as usize * 0x20,
        };
        if !c.reachable(&c.corb) || !c.reachable(&c.rirb) {
            return Err("no 64-bit DMA and the command rings are above 4 GiB".into());
        }
        c.reset()?;
        c.init_rings();
        Ok(c)
    }

    fn reachable(&self, buf: &DmaBuffer) -> bool {
        self.dma64 || buf.phys() + (buf.len() as u64) <= 1 << 32
    }

    fn read16(&self, reg: usize) -> u16 {
        read16(self.regs, reg)
    }

    fn write16(&self, reg: usize, v: u16) {
        write16(self.regs, reg, v)
    }

    /// Controller reset: stop DMA, pulse CRST, wait for the codecs to announce themselves.
    fn reset(&mut self) -> Result<(), String> {
        let regs = self.regs;
        write8(regs, CORBCTL, 0);
        write8(regs, RIRBCTL, 0);
        write32(regs, INTCTL, 0);
        write32(regs, GCTL, read32(regs, GCTL) & !GCTL_CRST);
        if !wait_until(pit::HZ / 10, || read32(regs, GCTL) & GCTL_CRST == 0) {
            return Err("controller did not enter reset".into());
        }
        delay_us(100);
        write32(regs, GCTL, read32(regs, GCTL) | GCTL_CRST);
        if !wait_until(pit::HZ / 10, || read32(regs, GCTL) & GCTL_CRST != 0) {
            return Err("controller did not leave reset".into());
        }
        // Codecs request a state change within 521 us of the reset ending (spec 4.3).
        task::sleep_ticks(2);
        Ok(())
    }

    fn init_rings(&mut self) {
        let regs = self.regs;
        // The largest ring size each one supports (capability bits 7:4: 2, 16, 256 entries).
        let pick = |cap: u8| if cap & 0x40 != 0 { (2u8, 256u16) } else if cap & 0x20 != 0 { (1, 16) } else { (0, 2) };
        let (code, entries) = pick(read8(regs, CORBSIZE));
        write8(regs, CORBSIZE, read8(regs, CORBSIZE) & !3 | code);
        self.corb_entries = entries;
        let corb = self.corb.phys();
        write32(regs, CORBLBASE, corb as u32);
        write32(regs, CORBUBASE, (corb >> 32) as u32);
        self.write16(CORBWP, 0);
        // Reset the read pointer; some controllers never show the bit, so do not insist.
        self.write16(CORBRP, CORBRP_RST);
        wait_until(pit::HZ / 100, || read16(regs, CORBRP) & CORBRP_RST != 0);
        self.write16(CORBRP, 0);
        wait_until(pit::HZ / 100, || read16(regs, CORBRP) & CORBRP_RST == 0);
        write8(regs, CORBCTL, RING_RUN);

        let (code, entries) = pick(read8(regs, RIRBSIZE));
        write8(regs, RIRBSIZE, read8(regs, RIRBSIZE) & !3 | code);
        self.rirb_entries = entries;
        let rirb = self.rirb.phys();
        write32(regs, RIRBLBASE, rirb as u32);
        write32(regs, RIRBUBASE, (rirb >> 32) as u32);
        self.write16(RIRBWP, RIRBWP_RST);
        self.rirb_rp = 0;
        // Response interrupt count: the controller pauses the CORB after this many
        // responses until RIRBSTS is cleared, which `command` does after each one.
        self.write16(RINTCNT, 0xFF);
        write8(regs, RIRBSTS, 0x05);
        write8(regs, RIRBCTL, RING_RUN);
    }

    /// Send one verb to node `nid` of codec `cad`; the codec's response.
    fn command(&mut self, cad: u8, nid: u8, verb: u32) -> Option<u32> {
        let cmd = (cad as u32) << 28 | (nid as u32) << 20 | (verb & 0xF_FFFF);
        if !self.immediate {
            if let Some(r) = self.ring_command(cad, cmd) {
                return Some(r);
            }
            // No answer through the rings: stop them and use the immediate registers.
            write8(self.regs, CORBCTL, 0);
            write8(self.regs, RIRBCTL, 0);
            self.immediate = true;
        }
        self.immediate_command(cmd)
    }

    fn ring_command(&mut self, cad: u8, cmd: u32) -> Option<u32> {
        let regs = self.regs;
        let wp = (self.read16(CORBWP) & 0xFF) as usize;
        let next = (wp + 1) % self.corb_entries as usize;
        let slot = self.corb.as_ptr().wrapping_add(next * 4);
        // SAFETY: entry `next` lies inside the 1 KiB CORB.
        unsafe { (slot as *mut u32).write_volatile(cmd) };
        flush_cache(slot, 4);
        self.write16(CORBWP, next as u16);
        let end = pit::ticks() + pit::HZ / 20 + 2;
        loop {
            let wp = (self.read16(RIRBWP) & 0xFF) as u16 % self.rirb_entries;
            while self.rirb_rp != wp {
                self.rirb_rp = (self.rirb_rp + 1) % self.rirb_entries;
                let entry = self.rirb.as_ptr().wrapping_add(self.rirb_rp as usize * 8);
                flush_cache(entry, 8);
                // SAFETY: entry rirb_rp lies inside the 2 KiB RIRB; the device wrote it.
                let (response, extended) = unsafe { ((entry as *const u32).read_volatile(), (entry.add(4) as *const u32).read_volatile()) };
                write8(regs, RIRBSTS, 0x05);
                // Unsolicited responses (jack events) and other codecs' answers are skipped.
                if extended & 0x10 == 0 && (extended & 0xF) as u8 == cad {
                    return Some(response);
                }
            }
            if pit::ticks() >= end {
                return None;
            }
            core::hint::spin_loop();
        }
    }

    fn immediate_command(&mut self, cmd: u32) -> Option<u32> {
        let regs = self.regs;
        if !wait_until(pit::HZ / 20, || read16(regs, ICIS) & ICIS_BUSY == 0) {
            return None;
        }
        self.write16(ICIS, ICIS_VALID);
        write32(regs, ICOI, cmd);
        self.write16(ICIS, ICIS_BUSY);
        wait_until(pit::HZ / 20, || read16(regs, ICIS) & ICIS_VALID != 0).then(|| read32(regs, ICII))
    }
}

impl Drop for Controller {
    /// A controller that is not used stops its command rings before their memory is freed.
    fn drop(&mut self) {
        write8(self.regs, CORBCTL, 0);
        write8(self.regs, RIRBCTL, 0);
        let regs = self.regs;
        wait_until(pit::HZ / 100, || read8(regs, CORBCTL) & RING_RUN == 0 && read8(regs, RIRBCTL) & RING_RUN == 0);
    }
}

/// What kind of output a pin is (configuration default, device field).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Jack {
    LineOut,
    Speaker,
    Headphone,
    Hdmi,
    DisplayPort,
    Spdif,
}

impl Jack {
    fn name(self) -> &'static str {
        match self {
            Jack::LineOut => "line out",
            Jack::Speaker => "speaker",
            Jack::Headphone => "headphone",
            Jack::Hdmi => "HDMI",
            Jack::DisplayPort => "DisplayPort",
            Jack::Spdif => "S/PDIF",
        }
    }

    fn digital(self) -> bool {
        matches!(self, Jack::Hdmi | Jack::DisplayPort | Jack::Spdif)
    }
}

#[derive(Clone)]
struct Widget {
    nid: u8,
    caps: u32,
    conns: Vec<u8>,
}

impl Widget {
    fn kind(&self) -> u8 {
        (self.caps >> 20 & 0xF) as u8
    }
}

/// One output pin and the widgets from it to a DAC: (node, connection index used to
/// reach the next node), pin first, DAC last.
#[derive(Clone)]
struct Path {
    jack: Jack,
    present: bool,
    hops: Vec<(u8, usize)>,
}

struct Codec {
    cad: u8,
    vendor: u32,
    afg_in_amp: u32,
    afg_out_amp: u32,
    widgets: Vec<Widget>,
    paths: Vec<Path>,
}

impl Codec {
    fn name(&self) -> String {
        let (v, d) = ((self.vendor >> 16) as u16, self.vendor as u16);
        let vendor = match v {
            0x10EC => return format!("Realtek ALC{d:x}"),
            0x1002 => "AMD/ATI",
            0x1022 => "AMD",
            0x8086 => "Intel",
            0x10DE => "NVIDIA",
            0x14F1 => "Conexant",
            0x111D | 0x8384 => "IDT/SigmaTel",
            0x11D4 => "Analog Devices",
            0x1106 => "VIA",
            0x1AF4 => "QEMU",
            0x434D => "C-Media",
            0x1013 => "Cirrus Logic",
            _ => "codec",
        };
        format!("{vendor} {v:04x}:{d:04x}")
    }

    fn widget(&self, nid: u8) -> Option<&Widget> {
        self.widgets.iter().find(|w| w.nid == nid)
    }

    fn analog(&self) -> bool {
        self.paths.iter().any(|p| !p.jack.digital())
    }

    /// The outputs this codec would play on.
    fn chosen(&self) -> Vec<&Path> {
        if self.analog() {
            return self.paths.iter().filter(|p| !p.jack.digital()).collect();
        }
        // HDMI/DisplayPort: the pins with a display attached, else all of them.
        let present: Vec<&Path> = self.paths.iter().filter(|p| p.present).collect();
        if present.is_empty() { self.paths.iter().collect() } else { present }
    }

    fn summary(&self) -> String {
        let mut jacks: Vec<String> = Vec::new();
        for p in self.chosen() {
            let name = format!("{}{}", p.jack.name(), if p.jack.digital() && p.present { " (connected)" } else { "" });
            if !jacks.contains(&name) {
                jacks.push(name);
            }
        }
        format!("{}: {}", self.name(), if jacks.is_empty() { "no outputs".into() } else { jacks.join(" + ") })
    }
}

fn param(c: &mut Controller, cad: u8, nid: u8, p: u32) -> u32 {
    c.command(cad, nid, GET_PARAM | p).unwrap_or(0)
}

/// Read a widget's connection list (short or long form, with ranges).
fn connections(c: &mut Controller, cad: u8, nid: u8) -> Vec<u8> {
    let len_param = param(c, cad, nid, P_CONN_LEN);
    let (len, long) = ((len_param & 0x7F) as usize, len_param & 0x80 != 0);
    let (per, bits) = if long { (2, 16) } else { (4, 8) };
    let mut list: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < len {
        let word = c.command(cad, nid, GET_CONN_LIST | i as u32).unwrap_or(0);
        for j in 0..per {
            if i + j >= len {
                break;
            }
            let entry = word >> (j * bits) & ((1 << bits) - 1);
            let (range, id) = (entry >> (bits - 1) != 0, (entry & ((1 << (bits - 1)) - 1)) as u8);
            match list.last() {
                Some(&prev) if range && id > prev => list.extend(prev + 1..=id),
                _ => list.push(id),
            }
        }
        i += per;
    }
    list
}

/// Discover codec `cad`: its audio function group, widgets and output paths.
fn probe_codec(c: &mut Controller, cad: u8) -> Option<Codec> {
    let vendor = c.command(cad, 0, GET_PARAM | P_VENDOR)?;
    let nodes = param(c, cad, 0, P_NODES);
    let (first, count) = ((nodes >> 16 & 0xFF) as u8, (nodes & 0xFF) as u8);
    let afg = (first..first.saturating_add(count)).find(|&n| param(c, cad, n, P_FUNCTION_TYPE) & 0xFF == 1)?;
    c.command(cad, afg, SET_POWER)?; // D0
    task::sleep_ticks(1);
    let nodes = param(c, cad, afg, P_NODES);
    let (first, count) = ((nodes >> 16 & 0xFF) as u8, (nodes & 0xFF) as u8);
    let mut codec = Codec {
        cad,
        vendor,
        afg_in_amp: param(c, cad, afg, P_IN_AMP),
        afg_out_amp: param(c, cad, afg, P_OUT_AMP),
        widgets: Vec::new(),
        paths: Vec::new(),
    };
    for nid in first..first.saturating_add(count) {
        let caps = param(c, cad, nid, P_WIDGET_CAPS);
        let conns = if caps & (1 << 8) != 0 { connections(c, cad, nid) } else { Vec::new() };
        codec.widgets.push(Widget { nid, caps, conns });
    }
    for w in codec.widgets.clone() {
        if w.kind() != W_PIN {
            continue;
        }
        let pincaps = param(c, cad, w.nid, P_PIN_CAPS);
        let config = c.command(cad, w.nid, GET_CONFIG_DEFAULT).unwrap_or(0);
        if pincaps & PINCAP_OUT == 0 || config >> 30 == 1 {
            continue; // not an output, or nothing connected to the pin
        }
        let jack = match (config >> 20 & 0xF, w.caps & WCAP_DIGITAL != 0) {
            (_, true) if pincaps & PINCAP_DP != 0 => Jack::DisplayPort,
            (_, true) if pincaps & PINCAP_HDMI != 0 => Jack::Hdmi,
            (0x4, _) | (0x5, _) => if pincaps & PINCAP_HDMI != 0 { Jack::Hdmi } else { Jack::Spdif },
            (_, true) => Jack::Spdif,
            (0x0, false) => Jack::LineOut,
            (0x1, false) => Jack::Speaker,
            (0x2, false) => Jack::Headphone,
            _ => continue,
        };
        // Presence detect, unless the jack has none or the BIOS marked it unusable.
        let present = pincaps & PINCAP_PRESENCE != 0
            && config >> 8 & 1 == 0
            && c.command(cad, w.nid, GET_PIN_SENSE).is_some_and(|s| s >> 31 != 0);
        let mut hops = Vec::new();
        if find_dac(&codec, w.nid, &mut hops, 0) {
            codec.paths.push(Path { jack, present, hops });
        }
    }
    Some(codec)
}

/// Depth-first search from `nid` to a DAC through mixers and selectors; on success
/// `hops` holds the path (each node with the index of the connection taken).
fn find_dac(codec: &Codec, nid: u8, hops: &mut Vec<(u8, usize)>, depth: usize) -> bool {
    let Some(w) = codec.widget(nid) else { return false };
    if w.kind() == W_DAC {
        hops.push((nid, 0));
        return true;
    }
    if depth > 0 && !matches!(w.kind(), W_MIXER | W_SELECTOR) || depth > 6 {
        return false;
    }
    for (i, &next) in w.conns.iter().enumerate() {
        if hops.iter().any(|&(n, _)| n == next) {
            continue;
        }
        hops.push((nid, i));
        if find_dac(codec, next, hops, depth + 1) {
            return true;
        }
        hops.pop();
    }
    false
}

/// Power up and unmute one output path and point its DAC at the stream.
fn configure_path(c: &mut Controller, codec: &Codec, path: &Path) {
    let cad = codec.cad;
    for (pos, &(nid, conn)) in path.hops.iter().enumerate() {
        let Some(w) = codec.widget(nid) else { continue };
        if w.caps & WCAP_POWER != 0 {
            c.command(cad, nid, SET_POWER);
        }
        let amp = |out: bool, c: &mut Controller| {
            let caps = if w.caps & WCAP_AMP_OVERRIDE != 0 {
                param(c, cad, nid, if out { P_OUT_AMP } else { P_IN_AMP })
            } else if out {
                codec.afg_out_amp
            } else {
                codec.afg_in_amp
            };
            // Gain at the 0 dB step (the caps' offset field), unmuted, both channels.
            caps & 0x7F
        };
        if w.caps & WCAP_OUT_AMP != 0 {
            let gain = amp(true, c);
            c.command(cad, nid, SET_AMP | 1 << 15 | 3 << 12 | gain);
        }
        let last = pos + 1 == path.hops.len();
        if !last {
            if w.caps & WCAP_IN_AMP != 0 && w.kind() != W_PIN {
                let gain = amp(false, c);
                c.command(cad, nid, SET_AMP | 1 << 14 | 3 << 12 | (conn as u32) << 8 | gain);
            }
            if w.conns.len() > 1 && w.kind() != W_MIXER {
                c.command(cad, nid, SET_CONN_SELECT | conn as u32);
            }
        }
        match w.kind() {
            W_PIN => {
                let pincaps = param(c, cad, nid, P_PIN_CAPS);
                let hp = path.jack == Jack::Headphone && pincaps & PINCAP_HP_DRIVE != 0;
                c.command(cad, nid, SET_PIN_CTL | 0x40 | if hp { 0x80 } else { 0 });
                if pincaps & PINCAP_EAPD != 0 {
                    c.command(cad, nid, SET_EAPD | 0x02);
                }
                if path.jack.digital() {
                    hdmi_infoframe(c, codec, nid);
                }
            }
            W_DAC => {
                c.command(cad, nid, SET_FORMAT | FORMAT_48K_16_STEREO as u32);
                c.command(cad, nid, SET_STREAM | (STREAM_TAG as u32) << 4);
                if w.caps & WCAP_DIGITAL != 0 {
                    c.command(cad, nid, SET_DIGI_CONV | 0x01); // digital output enable
                    c.command(cad, nid, SET_CONV_CHANNELS | 1); // two channels
                }
            }
            _ => {}
        }
    }
}

/// Tell an HDMI/DisplayPort sink it gets two-channel PCM: a CEA-861 audio infoframe in
/// the pin's data island packet buffer, sent with every frame (AMD codecs build the
/// infoframe themselves from a channel allocation).
fn hdmi_infoframe(c: &mut Controller, codec: &Codec, pin: u8) {
    let cad = codec.cad;
    if codec.vendor >> 16 == 0x1002 {
        c.command(cad, pin, ATI_SET_CHANNEL_ALLOCATION);
        return;
    }
    // Header (type 0x84, version 1, length 10), checksum, then the payload: channel count
    // minus one in byte 1, speaker allocation 0 (front left/right) in byte 4.
    let mut frame = [0x84u8, 0x01, 0x0A, 0, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    frame[3] = 0u8.wrapping_sub(frame.iter().fold(0u8, |s, &b| s.wrapping_add(b)));
    c.command(cad, pin, SET_DIP_INDEX);
    for b in frame {
        c.command(cad, pin, SET_DIP_DATA | b as u32);
    }
    c.command(cad, pin, SET_DIP_XMIT | 0xC0); // best effort, every frame
}

/// A probed controller waiting for [`start`] to choose between them.
struct Candidate {
    address: crate::pci::Address,
    controller: Controller,
    codecs: Vec<Codec>,
}

static CANDIDATES: SpinLock<Vec<Candidate>> = SpinLock::new(Vec::new());
static OUTPUT: SpinLock<Option<Output>> = SpinLock::new(None);

/// Reset an HD Audio controller and discover its codecs; returns a summary for the
/// device list. The controller is kept for [`start`].
pub fn probe(dev: &Device) -> Result<String, String> {
    let mut controller = Controller::new(dev)?;
    let present = read16(controller.regs, STATESTS) & 0x7FFF;
    controller.write16(STATESTS, present);
    let mut codecs = Vec::new();
    for cad in 0..15u8 {
        if present & 1 << cad != 0
            && let Some(codec) = probe_codec(&mut controller, cad)
        {
            codecs.push(codec);
        }
    }
    if codecs.is_empty() {
        return Err(if present == 0 { "no codecs attached".into() } else { "codecs did not answer".into() });
    }
    let mut detail = codecs.iter().map(Codec::summary).collect::<Vec<_>>().join("; ");
    if controller.immediate {
        detail.push_str(" (immediate commands)");
    }
    CANDIDATES.lock().push(Candidate { address: dev.address, controller, codecs });
    Ok(detail)
}

/// Choose the output (analog codecs first; `nova.audio=hdmi` prefers HDMI/DisplayPort),
/// configure it and start the polling task. Call once, after every controller was probed.
pub fn start() {
    let prefer_digital = crate::limine::EXECUTABLE_CMDLINE
        .get()
        .and_then(|c| c.split_whitespace().find_map(|a| a.strip_prefix("nova.audio=")))
        == Some("hdmi");
    let mut candidates = core::mem::take(&mut *CANDIDATES.lock());
    let score = |codec: &Codec| {
        let analog = codec.analog();
        let connected = codec.paths.iter().any(|p| p.present);
        ((analog != prefer_digital) as u8) * 2 + connected as u8
    };
    let best = candidates
        .iter()
        .enumerate()
        .flat_map(|(i, cand)| cand.codecs.iter().enumerate().map(move |(j, codec)| (i, j, codec)))
        .filter(|(_, _, codec)| !codec.paths.is_empty())
        .max_by_key(|(_, _, codec)| score(codec))
        .map(|(i, j, _)| (i, j));
    let Some((i, j)) = best else { return };
    let mut cand = candidates.swap_remove(i);
    let codec = cand.codecs.swap_remove(j);
    let c = &mut cand.controller;
    for path in codec.chosen() {
        configure_path(c, &codec, path);
    }
    let (bdl, buf) = (DmaBuffer::new(BDL_ENTRIES * 16, 128), DmaBuffer::new(BUF_SIZE, 128));
    if !c.reachable(&bdl) || !c.reachable(&buf) {
        crate::log::warn("hda: no 64-bit DMA and the stream buffer is above 4 GiB; no sound output");
        return;
    }
    let output = Output {
        regs: c.regs,
        sd: c.out_sd,
        bdl,
        buf,
        written: 0,
        played: 0,
        cleared: 0,
        last_lpib: 0,
        running: false,
        idle_since: 0,
        description: format!("{} on {}", codec.summary(), cand.address),
        codec_name: codec.name(),
    };
    crate::log::info(&format!("hda: output {}", output.description));
    *OUTPUT.lock() = Some(output);
    // The controller's command rings stay set up (and in use) for as long as Nova runs.
    core::mem::forget(cand);
    task::spawn("hda", poll_task);
}

/// The output stream: a cyclic DMA buffer and the cursors into it, as absolute byte
/// counts since the stream started (position modulo BUF_SIZE in the buffer).
struct Output {
    regs: u64,
    sd: usize,
    bdl: DmaBuffer,
    buf: DmaBuffer,
    /// End of the samples written so far.
    written: u64,
    /// The controller's position.
    played: u64,
    /// Everything before this has been zeroed after playing.
    cleared: u64,
    last_lpib: u32,
    running: bool,
    idle_since: u64,
    description: String,
    codec_name: String,
}

impl Output {
    fn sd_read8(&self, reg: usize) -> u8 {
        read8(self.regs, self.sd + reg)
    }

    fn sd_write8(&self, reg: usize, v: u8) {
        write8(self.regs, self.sd + reg, v)
    }

    /// Reset the stream descriptor and program it for a fresh start of the buffer.
    fn program(&mut self) {
        let (regs, sd) = (self.regs, self.sd);
        self.sd_write8(SD_CTL, self.sd_read8(SD_CTL) & !SD_CTL_RUN);
        wait_until(pit::HZ / 100, || read8(regs, sd + SD_CTL) & SD_CTL_RUN == 0);
        self.sd_write8(SD_CTL, SD_CTL_SRST);
        wait_until(pit::HZ / 100, || read8(regs, sd + SD_CTL) & SD_CTL_SRST != 0);
        self.sd_write8(SD_CTL, 0);
        wait_until(pit::HZ / 100, || read8(regs, sd + SD_CTL) & SD_CTL_SRST == 0);
        self.sd_write8(SD_STS, 0x1C); // clear BCIS, FIFOE, DESE

        let base = self.buf.phys();
        for i in 0..BDL_ENTRIES {
            let entry = self.bdl.as_ptr().wrapping_add(i * 16);
            // SAFETY: entry i lies inside the BDL; the stream is stopped.
            unsafe {
                (entry as *mut u64).write_volatile(base + (i * CHUNK) as u64);
                (entry.add(8) as *mut u32).write_volatile(CHUNK as u32);
                (entry.add(12) as *mut u32).write_volatile(0); // no interrupt on completion
            }
        }
        flush_cache(self.bdl.as_ptr(), self.bdl.len());
        self.buf.as_mut_slice().fill(0);
        flush_cache(self.buf.as_ptr(), BUF_SIZE);
        let bdl = self.bdl.phys();
        write32(regs, sd + SD_BDPL, bdl as u32);
        write32(regs, sd + SD_BDPU, (bdl >> 32) as u32);
        write32(regs, sd + SD_CBL, BUF_SIZE as u32);
        write16(regs, sd + SD_LVI, (BDL_ENTRIES - 1) as u16);
        write16(regs, sd + SD_FMT, FORMAT_48K_16_STEREO);
        // Stream number in CTL bits 23:20; no interrupts.
        self.sd_write8(SD_CTL + 2, STREAM_TAG << 4);
        self.written = 0;
        self.played = 0;
        self.cleared = 0;
        self.last_lpib = 0;
    }

    fn run(&mut self) {
        self.sd_write8(SD_CTL, self.sd_read8(SD_CTL) | SD_CTL_RUN);
        self.running = true;
        self.idle_since = pit::ticks();
    }

    fn stop(&mut self) {
        self.sd_write8(SD_CTL, self.sd_read8(SD_CTL) & !SD_CTL_RUN);
        self.running = false;
    }

    /// Follow the controller's position and zero what it has played.
    fn update(&mut self) {
        if !self.running {
            return;
        }
        let lpib = read32(self.regs, self.sd + SD_LPIB) % BUF_SIZE as u32;
        self.played += ((lpib + BUF_SIZE as u32 - self.last_lpib) % BUF_SIZE as u32) as u64;
        self.last_lpib = lpib;
        if self.written < self.played {
            // Underrun: the controller is playing zeros; new samples go just ahead of it.
            self.written = self.played + LEAD;
        }
        let end = self.played.saturating_sub(LAG);
        while self.cleared < end {
            let at = (self.cleared % BUF_SIZE as u64) as usize;
            let n = ((end - self.cleared) as usize).min(BUF_SIZE - at);
            self.buf.as_mut_slice()[at..at + n].fill(0);
            flush_cache(self.buf.as_ptr().wrapping_add(at), n);
            self.cleared += n as u64;
        }
        if self.written > self.played + LEAD {
            self.idle_since = pit::ticks();
        }
    }

    /// Bytes that can be written now without overwriting unplayed or unzeroed samples.
    fn room(&self) -> usize {
        let limit = (self.cleared + BUF_SIZE as u64).min(self.played + BUF_SIZE as u64 - LAG);
        limit.saturating_sub(self.written) as usize / FRAME * FRAME
    }

    fn write(&mut self, data: &[u8]) -> usize {
        if !self.running {
            self.program();
        }
        self.update();
        let n = data.len().min(self.room()) / FRAME * FRAME;
        let mut done = 0;
        while done < n {
            let at = ((self.written + done as u64) % BUF_SIZE as u64) as usize;
            let len = (n - done).min(BUF_SIZE - at);
            self.buf.as_mut_slice()[at..at + len].copy_from_slice(&data[done..done + len]);
            flush_cache(self.buf.as_ptr().wrapping_add(at), len);
            done += len;
        }
        self.written += n as u64;
        if !self.running && n > 0 {
            self.run();
        }
        n
    }

    fn queued(&self) -> usize {
        if self.running { self.written.saturating_sub(self.played) as usize } else { 0 }
    }
}

fn poll_task() {
    loop {
        let running = {
            let mut out = OUTPUT.lock();
            let Some(o) = out.as_mut() else { return };
            o.update();
            if o.running && pit::ticks() - o.idle_since > IDLE_STOP {
                o.stop();
            }
            o.running
        };
        task::sleep_ticks(if running { 5 } else { 50 });
    }
}

/// Whether an output is configured.
pub fn present() -> bool {
    OUTPUT.lock().is_some()
}

/// Queue interleaved stereo 16-bit samples at 48 kHz; returns how many samples (not
/// frames) were taken, which is less than given when the buffer is full.
pub fn write(samples: &[i16]) -> usize {
    let mut out = OUTPUT.lock();
    let Some(o) = out.as_mut() else { return 0 };
    let frames = samples.len() / CHANNELS;
    let mut bytes = Vec::with_capacity(frames.min(BUF_SIZE / FRAME) * FRAME);
    for s in &samples[..frames.min(BUF_SIZE / FRAME) * CHANNELS] {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    o.write(&bytes) / 2
}

/// Frames that [`write`] would take right now.
pub fn available() -> usize {
    let mut out = OUTPUT.lock();
    let Some(o) = out.as_mut() else { return 0 };
    if !o.running {
        return (BUF_SIZE - LAG as usize) / FRAME;
    }
    o.update();
    o.room() / FRAME
}

/// Frames written but not played yet.
pub fn queued() -> usize {
    let mut out = OUTPUT.lock();
    let Some(o) = out.as_mut() else { return 0 };
    o.update();
    o.queued() / FRAME
}

/// The codec's name, for the PulseAudio sink description.
pub fn codec_name() -> Option<String> {
    OUTPUT.lock().as_ref().map(|o| o.codec_name.clone())
}

/// One line for `audio` and the device list.
pub fn status() -> String {
    let mut out = OUTPUT.lock();
    let Some(o) = out.as_mut() else { return "no audio output".into() };
    o.update();
    let state = if o.running { format!("playing, {} ms queued", o.queued() / FRAME * 1000 / RATE as usize) } else { "idle".into() };
    let fifo = read16(o.regs, o.sd + SD_FIFOS);
    format!("{}; 48 kHz 16-bit stereo, {state}, FIFO {fifo} bytes", o.description)
}
