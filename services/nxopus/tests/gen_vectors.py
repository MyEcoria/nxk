#!/usr/bin/env python3
"""Opus test vectors for nxopus, made with the PC's libopus (ctypes):
CELT only (RESTRICTED_LOWDELAY), fullband, 48 kHz stereo, 20 ms frames,
synthetic signals; one mono-coded and two with 10 and 2.5 ms frames. Each
vector: per packet `u16 length, packet, u32 final range (the decoder's,
after the packet), u16 samples per channel, that many x 2 i16 decoded by
libopus`. A length of 0 is a lost packet, concealed (`opus_decode(NULL)`).

    python3 services/nxopus/tests/gen_vectors.py services/nxopus/tests/vectors
    python3 services/nxopus/tests/gen_vectors.py --decode CAPTURE OUT   # local only

`--decode` turns a capture (`u16 length, packet` records) into the same
format, for a check that is never committed.
"""
import ctypes, math, os, random, struct, sys

lib = ctypes.CDLL("libopus.so.0")
RATE, CH, N = 48000, 2, 960
APP_RESTRICTED_LOWDELAY = 2051
SET_BITRATE, SET_VBR, SET_BANDWIDTH, GET_FINAL_RANGE = 4002, 4006, 4008, 4031
SET_FORCE_CHANNELS = 4022
MAX = 5760
FULLBAND = 1105

lib.opus_encoder_create.restype = ctypes.c_void_p
lib.opus_decoder_create.restype = ctypes.c_void_p
lib.opus_encode.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_int16), ctypes.c_int,
                            ctypes.c_char_p, ctypes.c_int32]
lib.opus_decode.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int32,
                            ctypes.POINTER(ctypes.c_int16), ctypes.c_int, ctypes.c_int]
lib.opus_encoder_ctl.argtypes = [ctypes.c_void_p, ctypes.c_int]
lib.opus_decoder_ctl.argtypes = [ctypes.c_void_p, ctypes.c_int]


def encoder(bitrate, vbr, channels):
    err = ctypes.c_int()
    e = lib.opus_encoder_create(RATE, CH, APP_RESTRICTED_LOWDELAY, ctypes.byref(err))
    assert err.value == 0
    for req, v in ((SET_BITRATE, bitrate), (SET_VBR, vbr), (SET_BANDWIDTH, FULLBAND),
                   (SET_FORCE_CHANNELS, channels)):
        assert lib.opus_encoder_ctl(ctypes.c_void_p(e), req, ctypes.c_int(v)) == 0
    return e


def decoder():
    err = ctypes.c_int()
    d = lib.opus_decoder_create(RATE, CH, ctypes.byref(err))
    assert err.value == 0
    return d


def decode(d, packet, n=MAX):
    pcm = (ctypes.c_int16 * (MAX * CH))()
    n = lib.opus_decode(ctypes.c_void_p(d), packet or None, len(packet), pcm, n, 0)
    assert n > 0, n
    rng = ctypes.c_uint32()
    lib.opus_decoder_ctl(ctypes.c_void_p(d), GET_FINAL_RANGE, ctypes.byref(rng))
    return bytes(pcm)[:n * CH * 2], rng.value


def record(packet, rng, pcm):
    return (struct.pack("<H", len(packet)) + packet + struct.pack("<IH", rng, len(pcm) // 4)
            + pcm)


def vector(signal, frames, bitrate, vbr, n=N, channels=-1000, lost=()):
    e, d = encoder(bitrate, vbr, channels), decoder()
    out = b""
    for f in range(frames):
        pcm = (ctypes.c_int16 * (n * CH))()
        for i in range(n):
            l, r = signal(f * n + i)
            pcm[2 * i] = max(-32768, min(32767, int(l * 32767)))
            pcm[2 * i + 1] = max(-32768, min(32767, int(r * 32767)))
        buf = ctypes.create_string_buffer(1500)
        size = lib.opus_encode(ctypes.c_void_p(e), pcm, n, buf, 1500)
        assert size > 0, size
        packet = buf.raw[:size]
        assert packet[0] >> 3 == 28 + {120: 0, 240: 1, 480: 2, 960: 3}[n], packet[0]
        if f in lost:
            packet = b""
        dec, rng = decode(d, packet, n)
        out += record(packet, rng, dec)
    return out


def signals():
    rnd = random.Random(7)
    t = lambda i: i / RATE
    yield "tones", lambda i: (0.4 * math.sin(2 * math.pi * 440 * t(i)),
                              0.3 * math.sin(2 * math.pi * 3000 * t(i))), 96000, 1
    yield "noise", lambda i: (rnd.uniform(-0.5, 0.5), rnd.uniform(-0.5, 0.5)), 160000, 0
    yield "clicks", lambda i: ((0.9 if i % 4800 < 24 else 0.0),
                               (-0.9 if (i + 2400) % 4800 < 24 else 0.0)), 96000, 1
    yield "nearmono", lambda i: (0.5 * math.sin(2 * math.pi * 220 * t(i)),
                                 0.49 * math.sin(2 * math.pi * 220 * t(i) + 0.01)), 32000, 1
    yield "silence", lambda i: (0.0, 0.0), 96000, 1
    yield "mono", lambda i: (0.4 * math.sin(2 * math.pi * 330 * t(i)) + (0.5 if i % 9600 < 20 else 0),
                             0.4 * math.sin(2 * math.pi * 330 * t(i))), 48000, 1, N, 1
    yield "short10", lambda i: (0.4 * math.sin(2 * math.pi * 880 * t(i)) + (0.6 if i % 4000 < 16 else 0),
                                rnd.uniform(-0.2, 0.2)), 128000, 1, 480
    voice = lambda i: sum(0.12 / k * math.sin(2 * math.pi * k * (180 + 20 * math.sin(3 * t(i))) * t(i))
                          for k in range(1, 9))
    lost = (6, 11, 12, 18, 19, 20, 21, 22, 23, 24, 25, 33)
    yield "loss_voice", lambda i: (voice(i), voice(i + 37)), 64000, 1, N, -1000, lost
    yield "loss_music", lambda i: (0.3 * math.sin(2 * math.pi * 523 * t(i)) + rnd.uniform(-0.05, 0.05),
                                   0.3 * math.sin(2 * math.pi * 659 * t(i))), 96000, 1, N, -1000, lost
    yield "short2_5", lambda i: (0.5 * math.sin(2 * math.pi * 1500 * t(i)),
                                 0.3 * math.sin(2 * math.pi * 600 * t(i))), 128000, 0, 120
    yield "square", lambda i: ((1.2 if (i // 60) % 2 else -1.2), (1.2 if (i // 90) % 2 else -1.2)), 160000, 1
    yield "sweep", lambda i: (0.5 * math.sin(2 * math.pi * (100 + 9000 * t(i)) * t(i)),
                              0.5 * math.cos(2 * math.pi * (100 + 7000 * t(i)) * t(i))), 64000, 0


def main():
    if sys.argv[1] == "--decode":
        data, d, out, i = open(sys.argv[2], "rb").read(), decoder(), b"", 0
        while i < len(data):
            n = struct.unpack_from("<H", data, i)[0]
            packet = data[i + 2:i + 2 + n]
            i += 2 + n
            pcm, rng = decode(d, packet)
            out += record(packet, rng, pcm)
        open(sys.argv[3], "wb").write(out)
        return
    os.makedirs(sys.argv[1], exist_ok=True)
    for name, sig, *args in signals():
        frames = 40 if name.startswith("loss") else 20
        open(os.path.join(sys.argv[1], name + ".bin"), "wb").write(vector(sig, frames, *args))


if __name__ == "__main__":
    main()
