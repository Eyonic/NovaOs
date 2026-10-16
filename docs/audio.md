# Sound

Nova plays sound through its own Rust driver for Intel High Definition Audio
(`kernel/src/drivers/hda.rs`), and Linux programs reach it through the PulseAudio server built into
the kernel (`kernel/src/pulse.rs`). Output only: 48 kHz, 16-bit, stereo.

```
Linux program (libpulse)                    shell: tone <hz> <ms>
   │ native protocol over /run/user/1000/pulse/native        │
   ▼                                                          │
pulse.rs: streams → convert/resample → mix ──► hda::write ◄───┘
                                                  │
drivers/hda.rs: cyclic DMA buffer (BDL) ──► controller ──► codec: DAC → mixer → pin → jack
```

## The HD Audio driver

One driver covers every HD Audio controller (PCI class 04:03): Intel ICH/PCH, AMD FCH (`1022:1487`
on Zen 2/3 boards) and the HDMI/DisplayPort audio function of graphics cards (`1002:ab28` on the
RX 6800). It follows the Intel High Definition Audio Specification rev. 1.0a.

- **Controller**: reset through `GCTL.CRST`, codecs found in `STATESTS`; verbs go through the
  CORB/RIRB rings (256 entries, polled), falling back to the immediate command registers
  (`ICOI/ICII/ICIS`) if the rings never answer. No interrupts: like Nova's other drivers it is
  polled.
- **Codecs**: the audio function group is powered up and its widget graph read (capabilities,
  connection lists including ranges, pin configuration defaults). Every output pin (line out,
  speaker, headphone, HDMI/DisplayPort, S/PDIF) that the BIOS did not mark unconnected gets a
  depth-first path to a DAC through mixers and selectors.
- **Choosing an output**: `hda::start` runs after the PCI probe and picks one codec, analog
  before digital (`nova.audio=hdmi` on the kernel command line prefers HDMI/DisplayPort). On an
  analog codec *all* analog outputs are configured and fed from the same stream, so line out,
  headphones and speakers all play, whichever jack is used. On an HDMI codec, the pins with a
  display attached (presence detect), else all of them.
- **Path setup**: widgets to power state D0, output/input amplifiers unmuted at 0 dB (from the
  amplifier capabilities' offset), connection selects set, pin control out (+ headphone drive),
  EAPD on pins that have it. DACs get the stream format and stream tag 1. HDMI converters get
  digital enable and a channel count of two, and the pin a CEA-861 audio infoframe (AMD codecs
  instead take a channel allocation through their vendor verb and build the infoframe themselves).
- **Stream**: the first output stream descriptor, 48 kHz/16-bit/stereo, a 16-entry buffer
  descriptor list over one 64 KiB cyclic buffer (about 340 ms). `hda::write` copies samples ahead
  of the controller's position (`LPIB`, tracked across wraps as an absolute byte count); a kernel
  task (`hda`, every 5 ms) zeroes what has played, so an underrun plays silence rather than old
  samples. The stream starts on the first write and stops after a second of silence. Up to 2 KiB
  behind the reported position are left alone, because some controllers (AMD among them) report
  `LPIB` up to a FIFO ahead of what they fetched.
- **API**: `hda::write(&[i16]) -> usize` (interleaved stereo; returns samples taken),
  `hda::available()` and `hda::queued()` (frames), `hda::present()`, `hda::status()`.

Vendor details (from the hardware documentation and the behaviour Linux's `snd-hda-intel` is known
to need, written independently): AMD FCH and ATI southbridge controllers snoop DMA only when PCI
config `0x42` bits 0-2 are set to 2, which the driver does; Intel controllers get `TCSEL` (config
`0x44`) cleared to traffic class 0. The HDMI functions of AMD graphics cards do not snoop, so the
CPU writes back its cache lines (`clflush`) over every buffer it hands the controller and before
reading the response ring; this is done for every controller. Controllers without 64-bit
addressing (`GCAP.64OK`) are refused if their buffers land above 4 GiB.

Shell: `audio` prints the output and its state; `tone <hz> <ms>` plays a sine. `lspci` and the
setup screen show the codec and its outputs, e.g. `hda  Realtek ALC897: line out + headphone`.

## The PulseAudio server

`pulse.rs` speaks PulseAudio's native protocol (versions up to 35) over the socket libpulse looks
for. Without an HD Audio output it is a server with no sinks that refuses streams (enough to stop
Steam from retrying for ever); with one it offers the sink `nova_output` (48 kHz s16le stereo,
the default sink):

- `GET_SERVER_INFO`, `GET_SINK_INFO(_LIST)`, `LOOKUP_SINK`; sink volume and mute.
- `CREATE_PLAYBACK_STREAM`: u8, s16le/be, s32le and float32le samples, 1-8 channels (the first
  two are played), any rate from 8 to 192 kHz; buffer metrics from the client or defaults (200 ms
  target, a quarter of it per request, prebuffer = target).
- Flow control: the creation reply grants a target length of bytes; `REQUEST` packets ask for
  more as the queue drains. Audio arrives as data packets on the stream's channel (no shared
  memory is offered, so clients send samples inline). `STARTED` once the prebuffer is full,
  `UNDERFLOW` when a stream runs dry.
- `CORK`, `FLUSH`, `TRIGGER`, `PREBUF`, `DRAIN` (answered once the stream's samples have played),
  `DELETE_PLAYBACK_STREAM`, `GET_PLAYBACK_LATENCY`, `SET_PLAYBACK_STREAM_BUFFER_ATTR`, stream
  volume and mute, name and property-list updates.
- Mixing: every few milliseconds the server keeps about 50 ms queued in the HD Audio buffer. Each
  playing stream is converted to 48 kHz stereo (linear interpolation), scaled by its volume and
  the sink's (PulseAudio's cubic volume curve), summed and clipped.

Not implemented: recording (no sources), sample cache, sink-input listing (`pactl list
sink-inputs` is empty), moving streams, shared-memory transport.

## Tests

`scripts/test.sh --case audio` boots with QEMU's ICH6 controller and duplex codec recorded by the
`wav` audio backend, plays `tone 440 600`, then `patone` (`userland/pulse/patone.c`, a
libpulse-simple client) at 1000 Hz s16le 48 kHz and at 660 Hz float32le 44.1 kHz, and runs
`pactl info` and `pactl list sinks short` (the host's `pactl`, so the real libpulse parses the
replies). `audioich9` plays a tone through QEMU's ICH9 controller and output-only codec.
`scripts/check_audio.py` then checks the WAV: exactly the expected tones in order, each within
2 % of its frequency and of about the right length, and no sample-to-sample step larger than a
clean sine of that frequency and level could make (so a dropout or discontinuity fails). A
typical run:

```
audio check: tone 1: 440.2 Hz, 600 ms, peak 16000, largest step 921 (clean sine: 922); expected 440 Hz, 600 ms: ok
audio check: tone 2: 1000.0 Hz, 610 ms, peak 16383, largest step 2138 (clean sine: 2145); expected 1000 Hz, 600 ms: ok
audio check: tone 3: 659.6 Hz, 610 ms, peak 16382, largest step 1414 (clean sine: 1415); expected 660 Hz, 600 ms: ok
```

Not yet tried on real hardware: the AMD controller and Realtek codec of a Zen 3 board, and the
RX 6800's HDMI/DisplayPort audio. Codec-specific fix-ups that some boards need (Realtek
coefficient tweaks, pin overrides for front-panel jacks) are not implemented.
