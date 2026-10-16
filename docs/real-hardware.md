# Testing Nova on a real PC

QEMU cannot emulate the reference machine's Realtek network chips (RTL8168/RTL8125,
driven by Linux's `r8169`) or its Radeon GPU, so those drivers are proven only on the
machine itself. This page is the procedure. Nothing is installed: Nova runs from the USB
stick in a live session and keeps its files in memory.

## 1. Build the image

```sh
scripts/fetch-linux.sh                        # once: Linux source for the driver layer
NOVA_BOOT_MENU=1 FEATURES=linux-drivers scripts/build.sh   # build/nova.img
```

`linux-drivers` matters on a PC: without a PS/2 controller the keyboard and mouse work
only through Linux's USB stack (xHCI + HID), and the Realtek network chip only through
`r8169`. The image carries the Realtek and Radeon (Navi 21/22) firmware files from the
build host's linux-firmware package. `NOVA_BOOT_MENU=1` gives the stick a boot menu (see
below).

## 2. Write it to a USB stick

Find the stick's device name and make sure it is the stick (size, model) and not a disk
with data on it:

```sh
lsblk -o NAME,SIZE,MODEL,TRAN          # the stick shows TRAN "usb"
sudo dd if=build/nova.img of=/dev/sdX bs=4M conv=fsync status=progress   # sdX = the stick
```

Everything on the stick is overwritten.

## 3. Boot it

- Firmware setup: Secure Boot **off** (Nova's bootloader, Limine, is not signed); UEFI
  boot on (CSM/legacy is not needed).
- Pick the stick in the firmware's boot menu (often F8, F11 or F12 at power-on).
- Plug the network cable into the board's Realtek port.

The stick shows a boot menu for 5 seconds; the first entry boots when no key is pressed:

| Entry | Use it |
|---|---|
| **Nova (safe graphics)** | the default, and right for the **first boot**. The Radeon driver (`amdgpu`) starts the GPU but leaves the display alone (`amdgpu.dc=0`), so the firmware's screen stays visible |
| Nova | the full driver, display included. The screen may go dark when amdgpu takes the display over: Nova cannot hand scan-out back to its desktop yet |
| Nova (no GPU driver) | if a boot hangs at the "amdgpu" line (`nova.disable_driver=amdgpu`) |
| Nova (safe graphics, one CPU) | if a boot hangs or crashes after the "smp:" line: only the boot CPU, as before SMP (`nova.cpus=1`) |

Nova shows "Setting up this computer" with one line per PCI device and the driver that
took it, then (with `nova.mode=shell` or from the desktop's terminal) the shell.

## 4. What to check, and what to send back

Photos of the screen are enough. In the shell:

| Command | Shows |
|---|---|
| `lspci` | every PCI device and the driver bound to it (look for `r8169` on 10ec:8168 or 10ec:8125, `xhci_hcd`, the NVMe drive) |
| `ifconfig` | whether DHCP gave the Realtek port an address |
| `fetch http://example.com` | a real HTTP request over the Realtek chip |
| `dmesg r8169` | the Linux driver's own messages (chip version, firmware, link speed) |
| `dmesg amdgpu` | the Radeon driver: VBIOS, firmware, VRAM size, where it stopped |
| `dmesg firmware` | firmware files that were loaded or missing |
| `dmesg warning` | everything Nova warned about |
| `disks` | the NVMe drives and their partitions (read-only unless you run `install`) |

`dmesg` is the in-memory kernel log (the same lines QEMU shows on its serial console).

## Remote console

A PC has no serial port to watch, so Nova can use the network instead. Build the stick
with the address of another machine on the same network:

```sh
NOVA_BOOT_MENU=1 FEATURES=linux-drivers NOVA_CMDLINE="nova.mode=shell nova.remote=192.168.1.97:5599" scripts/build.sh
```

and listen there before booting: `nc -lk 5599` (or `socat -,raw,echo=0 TCP-LISTEN:5599,reuseaddr`).
Once DHCP gives Nova an address, it connects out, sends the whole kernel log, and then
mirrors its serial console both ways: everything Nova prints arrives there, and what is
typed there reaches Nova's shell. Nova opens no port of its own; if the link drops it
reconnects every few seconds. If the network driver does not come up, nothing connects:
photograph the screen instead.

## Safety

The live session does not write to internal disks: Nova only writes to a disk when you
run `install` and pick it. The firmware settings above are the only change to the PC;
turn Secure Boot back on afterwards if you use it.
