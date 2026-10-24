#!/usr/bin/env python3
"""Writes tests/fixtures/*.h264 and *.crc: small clips encoded by x264 the way
the Luna server does (High profile, CABAC, 8x8 transform, no B frames, three
references, several slices, no weighted prediction), and the CRC32 of each
decoded I420 frame as ffmpeg decodes it. Needs ffmpeg with libx264.

    tools/mkfixtures.py
"""
import subprocess, zlib, os

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "tests", "fixtures")

# name, size, frames, x264 parameters
CLIPS = [
    ("intra_2slices", (160, 96), 4, "slices=2:keyint=1"),
    ("p_1slice", (192, 128), 12, "slices=1:keyint=8"),
    ("p_4slices", (192, 128), 12, "slices=4:keyint=8"),
    ("p_odd_size", (176, 112), 10, "slices=3:keyint=6"),
]

def run(*cmd):
    return subprocess.run(cmd, check=True, capture_output=True).stdout

os.makedirs(OUT, exist_ok=True)
for name, (w, h), frames, extra in CLIPS:
    path = os.path.join(OUT, name + ".h264")
    run("ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i",
        f"testsrc2=size={w}x{h}:rate=30,noise=alls=10:allf=t,format=yuv420p",
        "-frames:v", str(frames), "-c:v", "libx264", "-profile:v", "high", "-preset", "medium",
        "-x264-params", f"{extra}:bframes=0:ref=3:cabac=1:8x8dct=1:aud=0:weightp=0",
        "-bsf:v", "h264_mp4toannexb", "-f", "h264", path)
    raw = run("ffmpeg", "-hide_banner", "-loglevel", "error", "-i", path, "-f", "rawvideo", "-pix_fmt", "yuv420p", "-")
    size = w * h * 3 // 2
    with open(os.path.join(OUT, name + ".crc"), "w") as f:
        f.write(f"{w} {h}\n")
        for k in range(len(raw) // size):
            f.write(f"{zlib.crc32(raw[k * size:(k + 1) * size]):08x}\n")
    print(name, os.path.getsize(path), "bytes,", len(raw) // size, "frames")
