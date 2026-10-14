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
    aliases = []  # symlinks to files inside the tree: entries sharing the target's data
    real_root = os.path.realpath(root)
    for dirpath, _, names in os.walk(root):
        for n in names:
            full = os.path.join(dirpath, n)
            rel = os.path.relpath(full, root).replace(os.sep, "/")
            target = os.path.realpath(full)
            if os.path.islink(full) and os.path.isfile(target) and target.startswith(real_root + os.sep):
                aliases.append((rel, os.path.relpath(target, real_root).replace(os.sep, "/")))
            elif os.path.isfile(full):
                files.append((rel, full))
    files.sort()
    aliases.sort()
    header_size = 16 + sum(8 + 8 + 2 + len(p.encode()) for p, _ in files + aliases)
    offset = (header_size + 4095) & ~4095
    entries = []
    where = {}
    for path, full in files:
        size = os.path.getsize(full)
        entries.append((offset, size, path.encode()))
        where[path] = (offset, size)
        offset = (offset + size + 4095) & ~4095
    data_entries = len(entries)
    for path, target in aliases:
        if target in where:
            entries.append(where[target] + (path.encode(),))
    with open(out, "wb") as f:
        f.write(b"NOVAPAK1" + struct.pack("<Q", len(entries)))
        for off, size, p in entries:
            f.write(struct.pack("<QQH", off, size, len(p)) + p)
        for (off, size, _), (_, full) in zip(entries[:data_entries], files):
            f.seek(off)
            with open(full, "rb") as src:
                f.write(src.read())
        f.truncate(offset)
    print(f"{out}: {len(entries)} files, {offset >> 20} MiB")


if __name__ == "__main__":
    main()
