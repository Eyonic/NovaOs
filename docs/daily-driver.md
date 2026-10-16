# What Nova needs to be a daily driver

The checklist below started from a list DeepSeek drew up (2026-10-16) of what a home PC's
operating system must ship or support, based on what Windows 11, macOS, Ubuntu, Fedora and
SteamOS include out of the box. Its 97 items are merged (it listed some twice) and each is
compared with Nova's source. **Have**: works today (in QEMU at least). **Partial**: a
start, with the gap noted. **In progress**: being built now. **Missing**: nothing yet.
Priorities are DeepSeek's: *must*, *should*, *nice*.

## Built-in apps

| Item | Priority | Nova | Notes |
|---|---|---|---|
| Web browser | must | Have | Nova Browser (Chromium engine, Shields ad blocking) |
| File manager | must | Have | Files |
| Text editor | must | Have | Text Editor |
| Terminal | must | Have | Terminal, Nova shell + BusyBox `sh` |
| Task manager | must | Have | System Monitor |
| Settings | must | Have | Settings |
| Screenshot tool | must | Partial | only the shell's `wayland save`; needs a Print Screen key and a capture UI |
| PDF viewer | must | Have | PdfCraft (Craft App Suite, `NOVA_CRAFT=1`, see [craft-apps.md](craft-apps.md)) |
| Image viewer | must | Partial | PhotoCraft opens PNG, JPEG, WebP, PSD... (`NOVA_CRAFT=1`); no light viewer app |
| Video player | must | Missing | needs codecs (below) and a player; mpv runs on Linux ABIs |
| Calculator | must | Missing | small native app |
| Archive manager | must | Partial | BusyBox `tar`, `unzip`, `gzip` in the terminal; no app |
| Software store / updates | must | Missing | installing Linux apps (Flatpak) and updating Nova itself |
| Office suite | must | Have | WordCraft, GridCraft, DeckCraft (Craft App Suite, `NOVA_CRAFT=1`): .docx, .xlsx, .pptx; software rendering, no clipboard with other programs yet |
| Music player | should | Missing | |
| Screen recorder | should | Missing | |
| Calendar, clock/alarms, notes | should | Missing | small native apps |
| Mail client | should | Missing | web mail works in the browser |
| Password manager / keyring | should | Missing | |
| Backup | should | Missing | |
| Assistant | (Nova's own) | Have | Nova Assistant on DeepSeek, or Claude Code |

## Hardware and system

| Item | Priority | Nova | Notes |
|---|---|---|---|
| Multi-core | must | In progress | all cores run programs; kernel serialised by one lock first (docs/smp.md) |
| GPU drivers (AMD) | must | In progress | amdgpu links, untested on the RX 6800 (docs/linux-drivers.md) |
| Vulkan / OpenGL | must | In progress | `/dev/dri` for Mesa (radeonsi, radv); software rendering today |
| NVIDIA / Intel graphics | must | Missing | nouveau/i915 would use the same Linux driver layer as amdgpu |
| Audio output | must | Have | Intel HDA (incl. AMD), AC97; a PulseAudio server in the kernel |
| Audio input (microphone) | must | Missing | HDA capture streams and a Pulse source |
| Headsets, USB audio | must | Missing | Linux `snd-usb-audio` via the driver layer |
| Ethernet | should | Have | e1000 (QEMU), Realtek r8169 (Linux driver, untested on the PC) |
| Wi-Fi | must | Missing | Linux `iwlwifi`/`mt76` + mac80211 + a WPA supplicant |
| Bluetooth (and audio) | must | Missing | Linux btusb + a stack |
| USB keyboard and mouse | must | Have | Linux xHCI + HID |
| USB storage hot-plug | must | Missing | Linux usb-storage/uas + SCSI disk, then mounting |
| Webcam | must | Missing | Linux uvcvideo + V4L2 for programs |
| Game controllers | should | Missing | Linux xpad/hid-generic + evdev (`/dev/input`) for programs |
| Printing / scanning | should | Missing | CUPS (Linux userland) once networking and USB are complete |
| Multi-monitor | should | Missing | needs the GPU's display (amdgpu KMS) |
| HiDPI scaling | should | Partial | design tokens have a scale, no per-display setting |
| HDR | nice | Missing | |
| Sleep / suspend | must | Missing | ACPI S3 and device suspend/resume |
| Power management | must | Partial | ACPI shutdown and reboot; no CPU frequency scaling or idle states |
| Time sync | must | Missing | RTC only; needs NTP |
| Fonts and languages | must | Partial | Latin text; needs fallback fonts (CJK, emoji) and translations |
| Input methods, emoji | should | Missing | |
| Accessibility (screen reader, magnifier, high contrast, sticky keys, on-screen keyboard) | must | Missing | |
| Notifications | must | Missing | |
| Clipboard | must | Partial | shared by native apps; Wayland `wl_data_device` for Linux apps |
| Drag and drop | must | Partial | window dragging; no file drag and drop between apps |
| User accounts, login, lock screen | must | Missing | one user, no password or lock |
| Disk encryption | should | Missing | |
| Firewall | should | Partial | Nova listens on no ports by default; no rules |
| VPN | should | Missing | WireGuard would fit in the kernel |
| SMB / network drives | should | Missing | |
| SSH client, remote desktop | should | Partial | BusyBox has no ssh; OpenSSH (Linux) would run |
| Dark mode, themes, wallpaper | should | Partial | dark theme and wallpaper; no light theme |
| Virtualisation | nice | Missing | |

## Programs people expect

| Item | Priority | Nova | Notes |
|---|---|---|---|
| Steam | must | Partial | reaches its sign-in window; sign-in, library, downloads next |
| Proton (Windows games) | must | Missing | runs inside Steam once Vulkan works |
| Discord, Spotify, Zoom | must | Missing | Electron/CEF apps: the same Chromium path Steam uses; Zoom also needs camera and microphone |
| Firefox / Chrome | must | Partial | the Nova Browser is Chromium; stock Firefox/Chrome are Linux programs that should run like Steam |
| Video codecs (H.264, HEVC, AV1), hardware decode | must | Missing | software decode via FFmpeg in programs; hardware decode needs VA-API on amdgpu |
| DRM video (Netflix, Prime) | should | Missing | Widevine in a Chromium build |
| Flatpak | should | Missing | uses bubblewrap and namespaces, which Nova has |
| Wine (Windows programs) | should | Missing | after Proton |
| OBS, VLC, GIMP, Inkscape, LibreOffice | should/nice | Missing | ordinary Linux programs; mostly blocked on GPU, multi-core and fonts |
| Cloud sync (Dropbox, Drive) | should | Missing | |

## Order of work

What unlocks the most at once, given the user's PC (Ryzen 7 5800X, RX 6800, Realtek
Ethernet):

1. The drivers on the real PC: r8169, HD Audio, amdgpu (tonight's USB-stick test).
2. Multi-core and the GPU path for Mesa (in progress), then Steam sign-in and a game.
3. USB storage, time sync, JPEG/PDF/video support with viewer apps, a calculator, and
   Print Screen screenshots: small pieces that make Nova usable day to day.
4. Accounts and a lock screen, notifications, suspend.
5. Wi-Fi and Bluetooth (mostly for laptops), webcam and microphone (video calls),
   Flatpak with a software store.
