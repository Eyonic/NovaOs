//! Live session vs installed system, and the installer.
//!
//! * Installed: the boot disk (identified by the GPT disk GUID Limine reports) has a
//!   NovaFS data partition. It is mounted as the system filesystem.
//! * Live: booted from the installation image. A RAM disk holds the filesystem until
//!   Nova is installed onto another disk.
//!
//! Installing writes a new GPT to the target disk with two partitions: an EFI System
//! Partition copied sector by sector from the boot disk's ESP (Limine, its config and
//! the kernel), and a NovaFS data partition that receives the live session's files.
//! The installer never writes to the disk it booted from.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;

use crate::fs::{self, NovaFs};
use crate::storage::{self, DiskInfo, RamDisk, ESP_TYPE, NOVA_DATA_TYPE};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Live,
    Installed,
}

static MODE: crate::sync::SpinLock<Mode> = crate::sync::SpinLock::new(Mode::Live);

pub fn mode() -> Mode {
    *MODE.lock()
}

/// Smallest disk the installer accepts.
const MIN_TARGET_SECTORS: u64 = 256 * 2048;

/// Mount the system filesystem: the boot disk's data partition if there is one,
/// otherwise a fresh RAM disk. Returns a description for the boot log.
pub fn mount_system() -> String {
    for d in storage::list().into_iter().filter(|d| d.is_boot) {
        if let Some(p) = d.partitions.iter().find(|p| p.type_guid == NOVA_DATA_TYPE) {
            match fs::mount(d.index, p.first_lba) {
                Ok(()) => {
                    *MODE.lock() = Mode::Installed;
                    return format!("installed system, NovaFS on {} partition {}", d.name, p.index);
                }
                Err(e) => crate::println!("[nova] warning: data partition on {} unusable: {e}", d.name),
            }
        }
    }
    let ram = storage::add(Box::new(RamDisk::new(32 << 20)));
    let sectors = storage::list()[ram].sectors;
    let result = NovaFs::format(ram, 0, sectors, "live").and_then(|_| fs::mount(ram, 0));
    match result {
        Ok(()) => "live session, files kept in RAM until Nova is installed".into(),
        Err(e) => format!("live session WITHOUT a filesystem ({e})"),
    }
}

/// Disks Nova could be installed on: not the boot disk, large enough.
pub fn targets() -> alloc::vec::Vec<DiskInfo> {
    storage::list().into_iter().filter(|d| !d.is_boot && d.sectors >= MIN_TARGET_SECTORS && !d.name.starts_with("ram")).collect()
}

/// Install onto disk `target`. `progress(percent, message)` is called as it goes.
pub fn install(target: usize, mut progress: impl FnMut(u32, &str)) -> Result<(), String> {
    let disks = storage::list();
    let boot = disks.iter().find(|d| d.is_boot).ok_or("cannot tell which disk Nova booted from")?;
    let dest = targets().into_iter().find(|d| d.index == target).ok_or("not a valid installation target")?;
    if dest.index == boot.index {
        return Err("refusing to install over the boot disk".into());
    }
    let esp = boot.partitions.iter().find(|p| p.type_guid == ESP_TYPE).ok_or("boot disk has no EFI System Partition")?.clone();

    // Layout: ESP at 1 MiB, same size as the source; data partition from the next MiB to the end.
    let esp_first = 2048u64;
    let esp_last = esp_first + esp.sectors() - 1;
    let data_first = (esp_last + 1).next_multiple_of(2048);
    let data_last = dest.sectors - 34;
    if data_last <= data_first + 64 * 2048 {
        return Err("target disk too small".into());
    }

    progress(0, "writing partition table");
    storage::with_disk(dest.index, |d| {
        storage::write_gpt(&mut *d.dev, &[(ESP_TYPE, esp_first, esp_last, "NOVA ESP"), (NOVA_DATA_TYPE, data_first, data_last, "NOVA DATA")])
    })
    .ok_or("target disk vanished")??;

    // Copy the ESP in 1 MiB chunks.
    let total = esp.sectors();
    let chunk = 2048u64;
    let mut buf = vec![0u8; chunk as usize * 512];
    let mut done = 0;
    while done < total {
        let n = chunk.min(total - done);
        let bytes = n as usize * 512;
        storage::with_disk(boot.index, |d| d.dev.read(esp.first_lba + done, &mut buf[..bytes])).ok_or("boot disk vanished")??;
        storage::with_disk(dest.index, |d| d.dev.write(esp_first + done, &buf[..bytes])).ok_or("target disk vanished")??;
        done += n;
        progress((done * 80 / total) as u32, "copying boot partition (Limine + kernel)");
    }

    progress(85, "creating NovaFS data partition");
    NovaFs::format(dest.index, data_first, data_last - data_first + 1, "nova")?;
    let mut target_fs = NovaFs::open(dest.index, data_first)?;
    let files = fs::snapshot();
    for (i, (name, data)) in files.iter().enumerate() {
        target_fs.write(name, data)?;
        progress(85 + (i as u32 * 10 / files.len().max(1) as u32), "copying your files");
    }
    let (y, mo, d, h, mi, _) = crate::rtc::now();
    target_fs.write("system/installed", format!("installed {y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC\n").as_bytes())?;
    storage::with_disk(dest.index, |d| d.dev.flush()).ok_or("target disk vanished")??;
    storage::rescan(dest.index);
    progress(100, "done");
    Ok(())
}
