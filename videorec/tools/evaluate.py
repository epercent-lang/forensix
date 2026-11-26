#!/usr/bin/env python3
"""Runs videorec on every scenario from make_images.py and scores the result.

Usage: evaluate.py WORKDIR [path/to/videorec]

For each expected original video it reports:
  exact   - an output file is byte-identical to the original
  frames  - share of the original's decoded video frames that appear,
            bit-identical after decoding, in some output (ffmpeg framemd5)
and per scenario the number of outputs that match no original (noise).
Exit status is non-zero if any scenario falls below its threshold.
"""

import hashlib
import json
import os
import subprocess
import sys


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def frame_hashes(path):
    r = subprocess.run(["ffmpeg", "-v", "quiet", "-i", path, "-map", "0:v:0", "-f", "framemd5", "-"],
                       capture_output=True, text=True)
    return [l.rsplit(",", 1)[1].strip() for l in r.stdout.splitlines() if l and not l.startswith("#")]


def audio_pcm(path):
    """First audio stream as 16-bit little-endian PCM, or b"" if none."""
    r = subprocess.run(["ffmpeg", "-v", "quiet", "-i", path, "-map", "0:a:0", "-f", "s16le", "-"],
                       capture_output=True)
    return r.stdout


def main():
    work = sys.argv[1]
    exe = sys.argv[2] if len(sys.argv) > 2 else os.path.join(os.path.dirname(__file__), "..", "target", "release", "videorec")
    truth = json.load(open(os.path.join(work, "truth.json")))
    vdir = os.path.join(work, "videos")
    orig_frames = {}
    failed = False
    rows = []
    for scen, t in truth.items():
        out = os.path.join(work, f"out_{scen}")
        subprocess.run(["rm", "-rf", out], check=True)
        ref = ["--reference", os.path.join(vdir, "ref.mp4")] if t.get("reference") else []
        subprocess.run([exe, "recover", os.path.join(work, f"{scen}.img"), "-o", out, *ref],
                       check=True, stdout=subprocess.DEVNULL)
        outputs = [os.path.join(out, f) for f in os.listdir(out) if not f.startswith("report.")]
        out_sha = {sha(p): p for p in outputs}
        out_frames = {p: set(frame_hashes(p)) for p in outputs}
        used = set()
        for name in t["expect"]:
            src = os.path.join(vdir, name)
            if name not in orig_frames:
                # Unplayable originals (E.mp4 has no moov) are scored
                # against the complete recording they were cut from.
                full = src.replace(".mp4", "_full.mp4")
                orig_frames[name] = frame_hashes(full if os.path.exists(full) else src)
            of = orig_frames[name]
            exact = sha(src) in out_sha
            if exact:
                used.add(out_sha[sha(src)])
            got = set()
            for p, fr in out_frames.items():
                inter = fr.intersection(of)
                if inter:
                    used.add(p)
                    got |= inter
            share = len(got) / len(of) if of else 0.0
            need = t.get("min_share", {}).get(name, 1.0)
            ok = share >= need
            failed |= not ok
            rows.append((scen, name, "yes" if exact else "no", f"{len(got)}/{len(of)}", f"{share:.0%}", "PASS" if ok else f"FAIL (<{need:.0%})"))
        if t.get("audio"):
            sound = b"".join(audio_pcm(p) for p in outputs)
            for name, need in t["audio"].items():
                ref = open(os.path.join(vdir, name), "rb").read()
                # Only chunks the recording actually held: up to the pass length.
                chunks = [ref[i:i + 1024] for i in range(0, len(ref), 1024)]
                chunks = chunks[:t.get("audio_chunks", {}).get(name, len(chunks))]
                found = sum(1 for c in chunks if c in sound)
                share = found / len(chunks)
                ok = share >= need
                failed |= not ok
                rows.append((scen, name, "", f"{found}/{len(chunks)}", f"{share:.0%}", "PASS" if ok else f"FAIL (<{need:.0%})"))
        noise = [os.path.basename(p) for p in outputs if p not in used]
        rows.append((scen, "(unmatched outputs)", "", str(len(noise)), "", ", ".join(noise)[:60]))
    w = [max(len(str(r[i])) for r in rows) for i in range(6)]
    head = ("scenario", "original", "exact", "frames", "share", "result")
    print("  ".join(h.ljust(w[i]) for i, h in enumerate(head)))
    for r in rows:
        print("  ".join(str(c).ljust(w[i]) for i, c in enumerate(r)))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
