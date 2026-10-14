//! Driver registry: matches PCI devices against Nova's built-in Rust drivers.
//!
//! [`probe_all`] scans the bus once, binds every device it has a driver for, and
//! records the outcome per device. With FEATURES=linux-drivers, devices no Rust driver
//! claims are offered to the Linux drivers compiled in (crate::linuxkpi). The setup screen and `lspci`/`drivers`
//! commands show that record; the Claude agent receives it as context when asked
//! about hardware that has no driver yet.

pub mod ahci;
pub mod e1000;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::pci::{self, Device};
use crate::sync::SpinLock;

/// A network interface card as the network stack sees it.
pub trait Nic: Send {
    fn mac(&self) -> [u8; 6];
    fn link_up(&self) -> bool;
    /// One received Ethernet frame, if any (an empty Vec means a frame was dropped).
    fn receive(&mut self) -> Option<Vec<u8>>;
    /// Queue one Ethernet frame; false if the transmit ring is full.
    fn transmit(&mut self, frame: &[u8]) -> bool;
}

/// A disk: 512-byte sectors; buffers are whole sectors.
pub trait BlockDevice: Send {
    fn name(&self) -> String;
    fn sector_count(&self) -> u64;
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), String>;
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), String>;
    fn flush(&mut self) -> Result<(), String>;
}

#[derive(Clone)]
pub enum Status {
    /// A driver claimed the device and initialised it.
    Bound { driver: &'static str, detail: String },
    /// A driver matched but initialisation failed.
    Failed { driver: &'static str, error: String },
    /// Infrastructure that works without a driver (bridges, the host bridge, ...).
    NotNeeded,
    /// No driver for this device yet.
    Missing,
    /// No driver in this kernel, but a Rust port was drafted (for the next build).
    Ported { detail: String },
}

#[derive(Clone)]
pub struct Record {
    pub device: Device,
    pub status: Status,
}

impl Record {
    pub fn describe(&self) -> String {
        let d = &self.device;
        format!("{} {:04x}:{:04x} {} {}", d.address, d.vendor, d.device, pci::vendor_name(d.vendor), d.class_name())
    }
}

static RECORDS: SpinLock<Vec<Record>> = SpinLock::new(Vec::new());

/// Devices the probe produced, handed to the subsystems that own them.
pub struct Probed {
    pub nics: Vec<Box<dyn Nic>>,
    pub disks: Vec<Box<dyn BlockDevice>>,
}

/// Scan PCI and bind drivers. `progress` is called once per device, in bus order.
pub fn probe_all(mut progress: impl FnMut(&Record)) -> Probed {
    let mut probed = Probed { nics: Vec::new(), disks: Vec::new() };
    let mut records = Vec::new();
    #[cfg(feature = "linux-drivers")]
    crate::linuxkpi::init();
    for device in pci::scan() {
        let status = bind(&device, &mut probed);
        #[cfg(feature = "linux-drivers")]
        if matches!(status, Status::Missing) {
            crate::linuxkpi::add_pci(&device);
        }
        records.push(Record { device, status });
    }
    #[cfg(feature = "linux-drivers")]
    bind_linux(&mut records, &mut probed);
    for record in &records {
        progress(record);
    }
    *RECORDS.try_lock().expect("driver records busy") = records;
    probed
}

/// Run the Linux drivers' initcalls (they probe the devices offered above) and record
/// which devices they bound; their network devices join `probed.nics`.
#[cfg(feature = "linux-drivers")]
fn bind_linux(records: &mut [Record], probed: &mut Probed) {
    crate::linuxkpi::start();
    for r in records.iter_mut().filter(|r| matches!(r.status, Status::Missing)) {
        if let Some(name) = crate::linuxkpi::driver_for(&r.device) {
            let driver: &'static str = alloc::boxed::Box::leak(format!("linux/{name}").into_boxed_str());
            let detail = format!("Linux {} driver, built from unmodified source", env!("NOVA_LINUX_VERSION"));
            r.status = Status::Bound { driver, detail };
        }
    }
    for nic in crate::linuxkpi::take_nics() {
        probed.nics.push(Box::new(nic));
    }
    crate::linuxkpi::spawn_poller();
}

fn bind(dev: &Device, probed: &mut Probed) -> Status {
    if dev.vendor == 0x8086
        && let Some((_, model)) = e1000::DEVICE_IDS.iter().find(|(id, _)| *id == dev.device)
    {
        return match e1000::E1000::new(dev) {
            Ok(nic) => {
                let m = nic.mac();
                let detail = format!(
                    "Intel {model}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}",
                    m[0], m[1], m[2], m[3], m[4], m[5], if nic.link_up() { "up" } else { "down" }
                );
                probed.nics.push(Box::new(nic));
                Status::Bound { driver: "e1000", detail }
            }
            Err(e) => Status::Failed { driver: "e1000", error: e.into() },
        };
    }
    if (dev.class, dev.subclass, dev.prog_if) == (0x01, 0x06, 0x01) {
        return match ahci::probe(dev) {
            Ok(disks) => {
                let detail = if disks.is_empty() {
                    "no disks attached".into()
                } else {
                    disks.iter().map(|d| format!("{} {} MiB", d.name(), d.sector_count() / 2048)).collect::<Vec<_>>().join(", ")
                };
                probed.disks.extend(disks.into_iter().map(|d| Box::new(d) as Box<dyn BlockDevice>));
                Status::Bound { driver: "ahci", detail }
            }
            Err(e) => Status::Failed { driver: "ahci", error: e },
        };
    }
    match (dev.class, dev.subclass) {
        // Bridges and the host bridge need no driver; the ISA bridge's legacy devices
        // (PIC, PIT, PS/2, COM1, RTC) are driven directly by the kernel.
        (0x06, _) => Status::NotNeeded,
        // The boot display keeps working through the firmware framebuffer.
        (0x03, _) => Status::Bound { driver: "framebuffer", detail: "firmware framebuffer (GOP), no mode setting".into() },
        // SMBus has nothing Nova needs.
        (0x0C, 0x05) => Status::NotNeeded,
        _ => Status::Missing,
    }
}

/// Snapshot of the probe results.
pub fn records() -> Vec<Record> {
    RECORDS.try_lock().map(|r| r.clone()).unwrap_or_default()
}

/// Register a later outcome for a device (e.g. a storage driver bound after the probe).
pub fn update(address: pci::Address, status: Status) {
    if let Some(mut records) = RECORDS.try_lock()
        && let Some(r) = records.iter_mut().find(|r| r.device.address == address)
    {
        r.status = status;
    }
}
