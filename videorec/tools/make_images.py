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
    # Motion-JPEG recording (many CCTV DVRs) and one still photo.
    run(*common, "-f", "lavfi", "-i", "testsrc2=size=640x480:rate=10:duration=6,hue=h=200",
        "-c:v", "mjpeg", "-q:v", "5", "-pix_fmt", "yuvj420p", f"{vdir}/M.avi")
    # MPEG-4 Part 2 (XviD-style) recording, as in many older DVRs.
    run(*common, "-f", "lavfi", "-i", "testsrc2=size=704x480:rate=25:duration=8,hue=h=120",
        "-c:v", "mpeg4", "-vtag", "XVID", "-g", "25", "-q:v", "4", f"{vdir}/X.avi")
    run(*common, "-f", "lavfi", "-i", "testsrc2=size=1024x768:rate=1:duration=1",
        "-frames:v", "1", "-pix_fmt", "yuvj420p", f"{vdir}/photo.jpg")
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


# ------------------------------------------------- G-NET-style .jdr ring files

# Chunk layout as observed on a real card (see src/jdr.rs); written here from
# scratch so the test does not depend on any vendor data.
JDR_SLOT = 3 * MB
JDR_SECONDS_PER_SLOT = 5


def jdr_streams(vdir):
    """Per pass and channel: a raw H.264 stream split into access units."""
    common = ["ffmpeg", "-y", "-hide_banner", "-loglevel", "error"]
    out = {}
    for pas, dur in ((1, 20), (2, 12.5)):
        for ch, size in ((0, "640x360"), (1, "320x240"), (2, "320x240")):
            name = f"P{pas}_CH{ch}.h264"
            run(*common, "-f", "lavfi", "-i",
                f"testsrc2=size={size}:rate=30:duration={dur},hue=h={pas * 97 + ch * 41}",
                "-c:v", "libx264", "-preset", "veryfast", "-g", "30", "-bf", "0", "-b:v", "600k",
                "-x264-params", "aud=0:repeat-headers=1", "-pix_fmt", "yuv420p",
                os.path.join(vdir, name))
            data = open(os.path.join(vdir, name), "rb").read()
            out[(pas, ch)] = (size, split_access_units(data))
    return out


def split_access_units(data):
    """Annex B H.264 -> list of (is_idr, bytes) per frame; SEI dropped."""
    idx = []
    i = data.find(b"\x00\x00\x01")
    while i >= 0:
        idx.append(i + 3)
        i = data.find(b"\x00\x00\x01", i + 3)
    nals = []
    for k, s in enumerate(idx):
        e = idx[k + 1] - 3 if k + 1 < len(idx) else len(data)
        while e > s and data[e - 1] == 0:
            e -= 1
        nals.append(data[s:e])
    aus, cur, idr = [], b"", False
    for n in nals:
        t = n[0] & 0x1F
        if t == 6:
            continue
        cur += b"\x00\x00\x00\x01" + n
        idr |= t == 5
        if t in (1, 5):
            aus.append((idr, cur))
            cur, idr = b"", False
    return aus


def jdr_chunk(ch, tag, payload, fmt, t_ms):
    import datetime
    t = datetime.datetime(2026, 11, 1) + datetime.timedelta(milliseconds=t_ms)
    head = f"{ch:02d}{tag}".encode() + struct.pack("<II", len(payload), fmt)
    head += struct.pack("<8H", t.year, t.month, 0, t.day, t.hour, t.minute, t.second, t.microsecond // 1000)
    return head + payload


def jdr_audio(pas, seconds=60):
    """Deterministic test sound for one pass: a tone plus noise, so every
    1024-byte chunk is distinct."""
    import math
    import random
    rnd = random.Random(pas)
    f = 440 * pas
    return b"".join(struct.pack("<h", int(8000 * math.sin(2 * math.pi * f * i / 8000)) + rnd.randint(-500, 500))
                    for i in range(8000 * seconds))


def jdr_pass(streams, pas, start_ms):
    """Bytes of each slot for one pass: channels interleaved frame by frame,
    with audio and sensor chunks, JDR_SECONDS_PER_SLOT seconds per slot."""
    slots = []
    n = len(streams[(pas, 0)][1])
    pcm = jdr_audio(pas)
    audio_n = [0]
    per = 30 * JDR_SECONDS_PER_SLOT
    for s0 in range(0, n, per):
        body = bytearray()
        for i in range(s0, min(n, s0 + per)):
            t = start_ms + i * 1000 // 30
            for ch in (0, 1, 2):
                size, aus = streams[(pas, ch)]
                w, h = map(int, size.split("x"))
                idr, au = aus[i]
                body += jdr_chunk(ch, "VI" if idr else "VP", au, h << 20 | w << 8 | 30, t)
            # Audio as on the real card: 512 samples (1024 bytes) of 16-bit
            # PCM at 8 kHz every 64 ms, tag digit cycling 0-2.
            while start_ms + audio_n[0] * 64 <= t:
                a = audio_n[0]
                body += jdr_chunk(a % 3, "AD", pcm[a * 1024:(a + 1) * 1024], 0, start_ms + a * 64)
                audio_n[0] += 1
            if i % 10 == 0:
                body += jdr_chunk(0, "SE", os.urandom(12), 0, t)
        slot = bytearray(b"1NAJ" + bytes(0x1FC) + b"1VEJ" + bytes(0x800 - 0x204))
        slot += body
        assert len(slot) <= JDR_SLOT, len(slot)
        slots.append(bytes(slot))
    return slots


# ------------------------------------------------------------- GPT (UEFI spec)

def write_gpt(path, first, last):
    """Protective MBR + primary GPT with one Basic Data partition."""
    import uuid
    import zlib
    total = os.path.getsize(path) // 512
    mbr = bytearray(512)
    mbr[446:462] = bytes([0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF]) + struct.pack("<II", 1, min(total - 1, 0xFFFFFFFF))
    mbr[510:512] = b"\x55\xAA"
    entry = bytearray(128)
    entry[0:16] = uuid.UUID("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7").bytes_le
    entry[16:32] = uuid.uuid4().bytes_le
    entry[32:48] = struct.pack("<QQ", first, last)
    entry[56:56 + 8] = "NVR".encode("utf-16-le") + b"\0\0"
    table = bytes(entry) + bytes(128 * 127)
    hdr = bytearray(92)
    hdr[0:8] = b"EFI PART"
    struct.pack_into("<IIIIQQQQ", hdr, 8, 0x00010000, 92, 0, 0, 1, total - 1, 34, total - 34)
    hdr[56:72] = uuid.uuid4().bytes_le
    struct.pack_into("<QIII", hdr, 72, 2, 128, 128, zlib.crc32(table))
    struct.pack_into("<I", hdr, 16, zlib.crc32(bytes(hdr)))
    with open(path, "r+b") as f:
        f.write(mbr)
        f.write(bytes(hdr) + bytes(420))
        f.write(table)


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

    # 8. G-NET-style ring: 4 pre-allocated .jdr files. Pass 1 (20 s, three
    #    channels) filled all of them; pass 2 (12.5 s) has rewritten files 0
    #    and 1 and the first part of file 2. Nothing is deleted in the file
    #    system; pass-1 footage survives only behind pass 2's write position.
    img = os.path.join(work, "jdr_ring.img")
    fat32_image(img)
    run("mmd", "-i", img, "::/data")
    streams = jdr_streams(vdir)
    p1 = jdr_pass(streams, 1, 10 * 3600 * 1000)
    p2 = jdr_pass(streams, 2, 16 * 3600 * 1000)
    tmp = os.path.join(work, "jdr_tmp")
    os.makedirs(tmp, exist_ok=True)
    for k in range(4):
        slot = bytearray(JDR_SLOT)
        slot[:len(p1[k])] = p1[k]
        if k < 2:
            slot = bytearray(JDR_SLOT)
            slot[:len(p2[k])] = p2[k]
        elif k == 2:
            # Pass 2 (12.5 s) ended 2.5 s into this file, in the middle of a GOP of pass 1.
            slot[:len(p2[2])] = p2[2]
        fn = os.path.join(tmp, f"{k:08d}.jdr")
        open(fn, "wb").write(slot)
        run("mcopy", "-i", img, fn, f"::/data/{k:08d}.jdr")
    shutil.rmtree(tmp)
    # Pass 1 keeps roughly seconds 12.5-20 of 20; pass 2 is complete.
    for p in (1, 2):
        open(os.path.join(vdir, f"P{p}_audio.pcm"), "wb").write(jdr_audio(p))
    truth["jdr_ring"] = {"expect": [f"P{p}_CH{c}.h264" for p in (1, 2) for c in (0, 1, 2)], "live": [],
                         "min_share": {f"P1_CH{c}.h264": 0.3 for c in (0, 1, 2)},
                         # Share of the pass's audio chunks found, byte-exact, in the outputs' sound.
                         "audio": {"P1_audio.pcm": 0.3, "P2_audio.pcm": 0.95},
                         "audio_chunks": {"P1_audio.pcm": 20000 // 64 + 1, "P2_audio.pcm": 12500 // 64 + 1}}

    # 9. Raw Motion-JPEG frames (AVI header lost) between random data, next
    #    to a single still photo that must not become a video.
    img = os.path.join(work, "mjpeg_raw.img")
    with open(img, "wb") as f:
        f.write(os.urandom(2 * MB))
        f.write(open(v("photo.jpg"), "rb").read())
        f.write(os.urandom(MB))
        f.write(open(v("M.avi"), "rb").read()[8192:])
        f.write(os.urandom(MB))
    truth["mjpeg_raw"] = {"expect": ["M.avi"], "live": [], "min_share": {"M.avi": 0.9}}

    # 9b. MPEG-4 Part 2 frames (AVI header lost) between random data.
    img = os.path.join(work, "mpeg4_raw.img")
    with open(img, "wb") as f:
        f.write(os.urandom(2 * MB))
        f.write(open(v("X.avi"), "rb").read()[8192:])
        f.write(os.urandom(MB))
    truth["mpeg4_raw"] = {"expect": ["X.avi"], "live": [], "min_share": {"X.avi": 0.85}}

    # 10. NTFS volume in a GPT disk (CCTV export PCs, NVR USB backups),
    #     files deleted. Needs mkntfs and ntfs-3g (FUSE) to build.
    if shutil.which("mkntfs") and shutil.which("ntfs-3g"):
        vol = os.path.join(work, "ntfs_vol.tmp")
        mnt = os.path.join(work, "ntfs_mnt")
        if os.path.exists(vol):
            os.remove(vol)
        run("truncate", "-s", "96M", vol)
        run("mkntfs", "-F", "-q", "-Q", "-c", "4096", "-p", "2048", "-H", "0", "-S", "0", vol)
        os.makedirs(mnt, exist_ok=True)
        run("ntfs-3g", vol, mnt)
        try:
            os.makedirs(os.path.join(mnt, "NVR", "ch01"), exist_ok=True)
            for n in ("A.mp4", "B.mp4", "C.mp4", "E.mp4"):
                shutil.copy(v(n), os.path.join(mnt, "NVR", "ch01", n))
            for n in ("A.mp4", "C.mp4", "E.mp4"):
                os.remove(os.path.join(mnt, "NVR", "ch01", n))
        finally:
            run("umount", mnt)
        img = os.path.join(work, "ntfs_gpt.img")
        if os.path.exists(img):
            os.remove(img)
        run("truncate", "-s", "100M", img)
        write_gpt(img, 2048, 2048 + 196608 - 1)
        with open(img, "r+b") as f, open(vol, "rb") as g:
            f.seek(2048 * 512)
            f.write(g.read())
        os.remove(vol)
        truth["ntfs_gpt"] = {"expect": ["A.mp4", "C.mp4", "E.mp4"], "live": ["B.mp4"]}

    json.dump(truth, open(os.path.join(work, "truth.json"), "w"), indent=2)
    print("images written to", work)


if __name__ == "__main__":
    main()
