#!/usr/bin/env python3
"""Pack a directory tree into a Nova system pak (one Limine module).

Format (little-endian):
  magic "NOVAPAK1", u64 entry count, then per entry: u64 data offset, u64 size,
  u16 path length, path bytes (UTF-8, '/'-separated, relative)
  then file data, every file starting on a 4 KiB boundary (so the kernel can map
  pages of read-only files straight into programs).

Usage: mkpak.py <directory> <output.pak>
"""
import os
import struct
import sys


def main():
    root, out = sys.argv[1], sys.argv[2]
    files = []
    for dirpath, _, names in os.walk(root):
        for n in names:
            full = os.path.join(dirpath, n)
            if os.path.isfile(full):
                files.append((os.path.relpath(full, root).replace(os.sep, "/"), full))
    files.sort()
    header_size = 16 + sum(8 + 8 + 2 + len(p.encode()) for p, _ in files)
    offset = (header_size + 4095) & ~4095
    entries = []
    for path, full in files:
        size = os.path.getsize(full)
        entries.append((offset, size, path.encode()))
        offset = (offset + size + 4095) & ~4095
    with open(out, "wb") as f:
        f.write(b"NOVAPAK1" + struct.pack("<Q", len(entries)))
        for off, size, p in entries:
            f.write(struct.pack("<QQH", off, size, len(p)) + p)
        for (off, size, _), (_, full) in zip(entries, files):
            f.seek(off)
            with open(full, "rb") as src:
                f.write(src.read())
        f.truncate(offset)
    print(f"{out}: {len(entries)} files, {offset >> 20} MiB")


if __name__ == "__main__":
    main()
