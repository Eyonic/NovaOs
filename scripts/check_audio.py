#!/usr/bin/env python3
"""Check a WAV file recorded by QEMU's wav audio backend (scripts/test.sh audio cases).

    check_audio.py FILE HZ:MS [HZ:MS ...]

The recording must contain exactly the given tones, in order, separated by silence: each
one at the given frequency (within 2 %) and about the given length, without dropouts or
discontinuities (no sample-to-sample step larger than a clean sine of that frequency and
level could make). Prints one line per tone; exit status 1 on any mismatch.
"""
import struct
import sys

WINDOW_MS = 10


def read_wav(path):
    """(rate, channels, left-channel samples) of a 16-bit PCM WAV; tolerates a header
    whose sizes were never filled in (QEMU writes them when it exits)."""
    data = open(path, "rb").read()
    if data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise SystemExit(f"{path}: not a WAV file")
    at, rate, channels, bits = 12, 0, 0, 0
    while at + 8 <= len(data):
        cid, size = data[at:at + 4], struct.unpack("<I", data[at + 4:at + 8])[0]
        body = at + 8
        if cid == b"fmt ":
            _, channels, rate, _, _, bits = struct.unpack("<HHIIHH", data[body:body + 16])
        elif cid == b"data":
            pcm = data[body:]
            break
        at = body + size + (size & 1)
    else:
        raise SystemExit(f"{path}: no data chunk")
    if bits != 16:
        raise SystemExit(f"{path}: {bits}-bit samples, expected 16")
    frame = 2 * channels
    n = len(pcm) // frame
    left = struct.unpack(f"<{n * channels}h", pcm[:n * frame])[::channels]
    return rate, channels, left


def segments(samples, rate):
    """(start, end) sample ranges of the non-silent parts (10 ms windows, RMS > 1000)."""
    w = rate * WINDOW_MS // 1000
    loud = []
    for i in range(len(samples) // w):
        chunk = samples[i * w:(i + 1) * w]
        loud.append(sum(s * s for s in chunk) / w > 1000 ** 2)
    runs, start = [], None
    for i, on in enumerate(loud + [False]):
        if on and start is None:
            start = i
        elif not on and start is not None:
            if i - start >= 5:  # ignore blips shorter than 50 ms
                runs.append((start * w, i * w))
            start = None
    return runs


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    path, want = sys.argv[1], [tuple(map(int, a.split(":"))) for a in sys.argv[2:]]
    rate, channels, left = read_wav(path)
    found = segments(left, rate)
    print(f"audio check: {path}: {len(left) / rate:.2f} s at {rate} Hz, {channels} channels, {len(found)} tone(s)")
    ok = len(found) == len(want)
    if not ok:
        print(f"audio check: expected {len(want)} tone(s)")
    for i, ((start, end), (hz, ms)) in enumerate(zip(found, want)):
        # Measure away from the fades at either end.
        inner = left[start + rate // 50:end - rate // 50]
        crossings = sum(1 for a, b in zip(inner, inner[1:]) if (a < 0) != (b < 0))
        freq = crossings / 2 / (len(inner) / rate)
        peak = max(abs(s) for s in inner)
        step = max(abs(b - a) for a, b in zip(inner, inner[1:]))
        clean_step = 2 * 3.14159265 * hz / rate * peak
        length = (end - start) * 1000 / rate
        good = abs(freq - hz) <= hz * 0.02 and abs(length - ms) <= max(60, ms * 0.15) and step <= clean_step * 1.3 + 64
        ok &= good
        print(f"audio check: tone {i + 1}: {freq:.1f} Hz, {length:.0f} ms, peak {peak}, largest step {step} "
              f"(clean sine: {clean_step:.0f}); expected {hz} Hz, {ms} ms: {'ok' if good else 'MISMATCH'}")
    print("audio check: PASS" if ok else "audio check: FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
