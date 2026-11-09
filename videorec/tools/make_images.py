#!/usr/bin/env python3
"""Builds synthetic dashcam SD-card images with known ground truth.

Needs: ffmpeg (libx264, libx265), dosfstools (mkfs.vfat), mtools (mcopy, mdel),
exfatprogs (mkfs.exfat). No root or mounting is needed: FAT32 images are
edited with mtools, exFAT images with the small writer in this file.

Usage: make_images.py WORKDIR
Writes WORKDIR/videos/*, WORKDIR/<scenario>.img and WORKDIR/truth.json.
"""

import json
import os
import shutil
import struct
import subprocess
import sys

MB = 1024 * 1024


def run(*cmd, **kw):
    subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **kw)


def make_videos(vdir):
    os.makedirs(vdir, exist_ok=True)
    common = ["ffmpeg", "-y", "-hide_banner", "-loglevel", "error"]
    src = lambda dur, seed: ["-f", "lavfi", "-i", f"testsrc2=size=640x360:rate=30:duration={dur},hue=h={seed % 360}",
                              "-f", "lavfi", "-i", f"sine=frequency={seed}:duration={dur}"]
    x264 = ["-c:v", "libx264", "-preset", "veryfast", "-g", "30", "-bf", "0", "-b:v", "2M", "-pix_fmt", "yuv420p"]
    x265 = ["-c:v", "libx265", "-preset", "veryfast", "-x265-params", "keyint=30:bframes=0:log-level=error",
            "-b:v", "2M", "-pix_fmt", "yuv420p", "-tag:v", "hvc1"]
    aac = ["-c:a", "aac", "-b:a", "64k"]
    # Dashcam-like MP4: moov at the end, audio interleaved.
    run(*common, *src(8, 440), *x264, *aac, f"{vdir}/A.mp4")
    run(*common, *src(6, 550), *x264, *aac, f"{vdir}/B.mp4")
    run(*common, *src(6, 660), *x265, *aac, f"{vdir}/C.mp4")
    run(*common, *src(5, 770), *x264, "-c:a", "pcm_s16le", f"{vdir}/D.avi")
    run(*common, *src(10, 880), *x264, *aac, f"{vdir}/E_full.mp4")
    run(*common, *src(3, 990), *x264, *aac, f"{vdir}/ref.mp4")
    # E.mp4: recording cut by power loss — everything from moov on is missing.
    data = open(f"{vdir}/E_full.mp4", "rb").read()
    i = 0
    while i < len(data):
        size, typ = struct.unpack(">I4s", data[i:i + 8])
        if typ == b"moov":
            break
        i += size
    open(f"{vdir}/E.mp4", "wb").write(data[:i])
    # Filler files for fragmentation scenarios.
    for name, size in (("F1.bin", 1 * MB), ("F2.bin", 1 * MB), ("F3.bin", 2 * MB), ("G.bin", 1 * MB)):
        with open(f"{vdir}/{name}", "wb") as f:
            f.write(os.urandom(size))


# --------------------------------------------------------------- FAT32 (mtools)

def fat32_image(path, size_mb=128):
    if os.path.exists(path):
        os.remove(path)
    run("truncate", "-s", f"{size_mb}M", path)
    run("mkfs.vfat", "-F", "32", "-s", "8", "-n", "DASHCAM", path)
    run("mmd", "-i", path, "::/DCIM")


def mput(img, src, dst):
    run("mcopy", "-i", img, src, f"::/DCIM/{dst}")


def mdel(img, name):
    run("mdel", "-i", img, f"::/DCIM/{name}")


# ------------------------------------------------------------- exFAT (own writer)

class ExFat:
    """Minimal exFAT writer: contiguous files in the root directory.
    Follows the Microsoft exFAT File System Specification."""

    def __init__(self, path):
        self.f = open(path, "r+b")
        b = self.read(0, 512)
        self.bps = 1 << b[108]
        self.cs = self.bps << b[109]
        self.heap = struct.unpack_from("<I", b, 88)[0] * self.bps
        self.count = struct.unpack_from("<I", b, 92)[0]
        self.root = struct.unpack_from("<I", b, 96)[0]
        root = self.read(self.coff(self.root), self.cs)
        self.bitmap_cluster = self.bitmap_len = None
        for i in range(0, self.cs, 32):
            if root[i] == 0x81:
                self.bitmap_cluster, self.bitmap_len = struct.unpack_from("<I", root, i + 20)[0], struct.unpack_from("<Q", root, i + 24)[0]
        assert self.bitmap_cluster

    def read(self, off, n):
        self.f.seek(off)
        return self.f.read(n)

    def write(self, off, data):
        self.f.seek(off)
        self.f.write(data)

    def coff(self, c):
        return self.heap + (c - 2) * self.cs

    def bitmap(self):
        return bytearray(self.read(self.coff(self.bitmap_cluster), self.bitmap_len))

    def alloc(self, n):
        bm = self.bitmap()
        run_start, run_len = None, 0
        for c in range(self.count):
            used = bm[c // 8] >> (c % 8) & 1
            if used:
                run_start, run_len = None, 0
                continue
            if run_start is None:
                run_start = c
            run_len += 1
            if run_len == n:
                for k in range(run_start, run_start + n):
                    bm[k // 8] |= 1 << (k % 8)
                self.write(self.coff(self.bitmap_cluster), bm)
                return run_start + 2
        raise RuntimeError("no space")

    def free(self, first, n):
        bm = self.bitmap()
        for k in range(first - 2, first - 2 + n):
            bm[k // 8] &= ~(1 << (k % 8)) & 0xFF
        self.write(self.coff(self.bitmap_cluster), bm)

    @staticmethod
    def name_hash(name):
        h = 0
        for ch in name.upper().encode("utf-16-le"):
            h = ((0x8000 if h & 1 else 0) + (h >> 1) + ch) & 0xFFFF
        return h

    @staticmethod
    def set_checksum(entries):
        s = 0
        for i, b in enumerate(entries):
            if i in (2, 3):
                continue
            s = ((0x8000 if s & 1 else 0) + (s >> 1) + b) & 0xFFFF
        return s

    def add_file(self, name, data):
        n = max(1, -(-len(data) // self.cs))
        first = self.alloc(n)
        self.write(self.coff(first), data)
        units = name.encode("utf-16-le")
        nname = -(-len(name) // 15)
        ts = (44 << 25) | (5 << 21) | (17 << 16) | (13 << 11) | (45 << 5) | 15  # 2024-05-17 13:45:30
        fe = bytearray(32)
        fe[0] = 0x85
        fe[1] = 1 + nname
        struct.pack_into("<H", fe, 4, 0x20)
        struct.pack_into("<III", fe, 8, ts, ts, ts)
        se = bytearray(32)
        se[0] = 0xC0
        se[1] = 0x03  # AllocationPossible | NoFatChain
        se[3] = len(name)
        struct.pack_into("<H", se, 4, self.name_hash(name))
        struct.pack_into("<Q", se, 8, len(data))
        struct.pack_into("<I", se, 20, first)
        struct.pack_into("<Q", se, 24, len(data))
        names = []
        for k in range(nname):
            e = bytearray(32)
            e[0] = 0xC1
            chunk = units[k * 30:(k + 1) * 30]
            e[2:2 + len(chunk)] = chunk
            names.append(e)
        entries = fe + se + b"".join(names)
        struct.pack_into("<H", entries, 2, self.set_checksum(entries))
        root_off = self.coff(self.root)
        root = self.read(root_off, self.cs)
        slot = next(i for i in range(0, self.cs, 32) if root[i] == 0)
        assert slot + len(entries) <= self.cs, "root directory cluster full"
        self.write(root_off + slot, entries)
        return first, n

    def delete(self, name):
        """Deletes like a typical implementation: clear InUse bits, free clusters."""
        root_off = self.coff(self.root)
        root = bytearray(self.read(root_off, self.cs))
        i = 0
        while i < self.cs and root[i]:
            if root[i] == 0x85:
                sec = root[i + 1]
                nlen = root[i + 32 + 3]
                units = b"".join(bytes(root[i + 32 * k + 2:i + 32 * k + 32]) for k in range(2, sec + 1))
                if units[:nlen * 2].decode("utf-16-le") == name:
                    first = struct.unpack_from("<I", root, i + 32 + 20)[0]
                    size = struct.unpack_from("<Q", root, i + 32 + 24)[0]
                    for k in range(sec + 1):
                        root[i + 32 * k] &= 0x7F
                    self.write(root_off, root)
                    self.free(first, max(1, -(-size // self.cs)))
                    return
                i += 32 * (sec + 1)
                continue
            i += 32
        raise KeyError(name)

    def close(self):
        self.f.close()


def exfat_image(path, size_mb=128):
    if os.path.exists(path):
        os.remove(path)
    run("truncate", "-s", f"{size_mb}M", path)
    run("mkfs.exfat", "-c", "32K", "-L", "DASHCAM", path)
    return ExFat(path)


# ------------------------------------------------------------------- scenarios

def main():
    work = sys.argv[1]
    vdir = os.path.join(work, "videos")
    make_videos(vdir)
    v = lambda n: os.path.join(vdir, n)
    truth = {}

    # 1. Files deleted normally on FAT32.
    img = os.path.join(work, "fat32_deleted.img")
    fat32_image(img)
    for n in ("A.mp4", "B.mp4", "C.mp4", "D.avi"):
        mput(img, v(n), n)
    mdel(img, "A.mp4")
    mdel(img, "C.mp4")
    mdel(img, "D.avi")
    truth["fat32_deleted"] = {"expect": ["A.mp4", "C.mp4", "D.avi"], "live": ["B.mp4"]}

    # 2. Card quick-formatted after recording (FAT and directories wiped).
    img = os.path.join(work, "fat32_formatted.img")
    fat32_image(img)
    for n in ("A.mp4", "B.mp4", "C.mp4", "D.avi", "E.mp4"):
        mput(img, v(n), n)
    run("mkfs.vfat", "-F", "32", "-s", "8", "-n", "DASHCAM", img)
    truth["fat32_formatted"] = {"expect": ["A.mp4", "B.mp4", "C.mp4", "D.avi", "E.mp4"], "live": []}

    # 3. After a quick format, a fragmented recording (A split around 768 KiB
    #    of other data) and a recording whose first 300 KiB were overwritten (B).
    img = os.path.join(work, "fat32_fragmented.img")
    fat32_image(img)
    run("mkfs.vfat", "-F", "32", "-s", "8", "-n", "DASHCAM", img)
    with open(img, "r+b") as f:
        bs = f.read(512)
        bps, spc, res, nfats = struct.unpack_from("<HBHB", bs, 11)
        fatsz = struct.unpack_from("<I", bs, 36)[0]
        data0 = (res + nfats * fatsz) * bps
        cl = bps * spc
        a = open(v("A.mp4"), "rb").read()
        b = open(v("B.mp4"), "rb").read()
        pos = data0 + 100 * cl
        cut = 256 * cl  # 1 MiB
        f.seek(pos)
        f.write(a[:cut])
        f.write(os.urandom(192 * cl))  # another file's clusters in between
        f.write(a[cut:])
        pos = data0 + 2000 * cl
        f.seek(pos)
        f.write(os.urandom(75 * cl))  # 300 KiB overwritten by a newer file
        f.write(b[75 * cl:])
    truth["fat32_fragmented"] = {"expect": ["A.mp4", "B.mp4"], "live": [],
                                 "min_share": {"A.mp4": 1.0, "B.mp4": 0.5}}

    # 4. exFAT card (cards over 32 GB), files deleted.
    img = os.path.join(work, "exfat_deleted.img")
    fs = exfat_image(img)
    for n in ("A.mp4", "B.mp4", "C.mp4", "E.mp4"):
        fs.add_file(n, open(v(n), "rb").read())
    fs.delete("A.mp4")
    fs.delete("C.mp4")
    fs.delete("E.mp4")
    fs.close()
    run("fsck.exfat", "-n", img)  # raises if our writer produced an invalid volume
    truth["exfat_deleted"] = {"expect": ["A.mp4", "C.mp4", "E.mp4"], "live": ["B.mp4"]}

    # 5. No file system at all: raw recorder area (data written back to back).
    img = os.path.join(work, "raw_noFS.img")
    with open(img, "wb") as f:
        f.write(os.urandom(3 * MB))
        for n in ("A.mp4", "C.mp4"):
            data = open(v(n), "rb").read()
            # Strip the container header so only mdat payload remains, as when
            # file headers are overwritten.
            f.write(data[4096:])
            f.write(os.urandom(512 * 1024))
    truth["raw_noFS"] = {"expect": ["A.mp4", "C.mp4"], "live": [],
                         "min_share": {"A.mp4": 0.8, "C.mp4": 0.8}}

    # 6. AVI (Annex B stream) whose header and first frames were overwritten,
    #    next to an intact MP4 from the same camera (source of SPS/PPS).
    img = os.path.join(work, "avi_overwritten.img")
    fat32_image(img)
    run("mkfs.vfat", "-F", "32", "-s", "8", "-n", "DASHCAM", img)
    with open(img, "r+b") as f:
        bs = f.read(512)
        bps, spc, res, nfats = struct.unpack_from("<HBHB", bs, 11)
        fatsz = struct.unpack_from("<I", bs, 36)[0]
        data0 = (res + nfats * fatsz) * bps
        cl = bps * spc
        d = open(v("D.avi"), "rb").read()
        f.seek(data0 + 50 * cl)
        f.write(open(v("B.mp4"), "rb").read())
        f.seek(data0 + 1000 * cl)
        f.write(os.urandom(16 * cl))
        f.write(d[16 * cl:])
    truth["avi_overwritten"] = {"expect": ["B.mp4", "D.avi"], "live": [],
                                "min_share": {"D.avi": 0.6}}

    # 7. Recordings stored back to back with no slack between them
    #    (pre-allocated recorder areas); later files start mid-sector.
    img = os.path.join(work, "back_to_back.img")
    with open(img, "wb") as f:
        for n in ("B.mp4", "C.mp4", "A.mp4"):
            f.write(open(v(n), "rb").read())
    truth["back_to_back"] = {"expect": ["B.mp4", "C.mp4", "A.mp4"], "live": []}

    json.dump(truth, open(os.path.join(work, "truth.json"), "w"), indent=2)
    print("images written to", work)


if __name__ == "__main__":
    main()
