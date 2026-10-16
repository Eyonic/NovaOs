# Testing Nova on a real PC

QEMU cannot emulate the reference machine's Realtek network chips (RTL8168/RTL8125,
driven by Linux's `r8169`) or its Radeon GPU, so those drivers are proven only on the
machine itself. This page is the procedure. Nothing is installed: Nova runs from the USB
stick in a live session and keeps its files in memory.

## 1. Build the image

```sh
scripts/fetch-linux.sh                        # once: Linux source for the driver layer
FEATURES=linux-drivers scripts/build.sh       # build/nova.img
```

`linux-drivers` matters on a PC: without a PS/2 controller the keyboard and mouse work
only through Linux's USB stack (xHCI + HID), and the Realtek network chip only through
`r8169`. The image carries the Realtek firmware files from the build host's
linux-firmware package.

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
| `dmesg firmware` | firmware files that were loaded or missing |
| `dmesg warning` | everything Nova warned about |
| `disks` | the NVMe drives and their partitions (read-only unless you run `install`) |

`dmesg` is the in-memory kernel log (the same lines QEMU shows on its serial console).

## Safety

The live session does not write to internal disks: Nova only writes to a disk when you
run `install` and pick it. The firmware settings above are the only change to the PC;
turn Secure Boot back on afterwards if you use it.
