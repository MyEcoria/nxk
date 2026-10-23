# Tasks

Planned work, not started. Each entry says what is known; per
`AGENTS.md` section 7, propose the options before coding any of them.

## 1. `sd`: power-cycled re-inits fail

With the Erista's microSD connector loose (hardware run 19), only DAT0
reached the card: hekate warned "SD Card Issues Warning" and ran two
different cards in 1-bit mode (initial bus HS25 1bit, 3 init fails, then
no read/write errors). `sd` read no data on its 4-bit bus (DAT lines
idle high, card in state 4, ADMA stopped after the first descriptor
fetch). Reseating the connector fixed the Switch (run 20).

`recover()` in `user/sd/src/main.rs` now first drops card and host to a
1-bit bus in place (CMD13, CMD12 if the card is still in data state,
ACMD6 with width 0, HOST_CONTROL bit 1 clear), without a power cycle;
not yet run on hardware. Behind it come the other retries, now 4-bit
after 2 s off, 1-bit after 239 ms off, then 1.8 V. In run 19 (1.8 V
first) all three failed with EIO and the card stopped answering CMD13,
although hekate succeeded in 1-bit after its own power cycles. Why is
not known. One
difference: hekate gates the SDMMC1 clock in CAR when it powers the
slot off (`sdmmc_end`) and re-enables it, resetting the controller, on
the next init; `recover()` keeps the controller clocked throughout.

## 2. `joycon`: the right Joy-Con is not detected

On the Erista tried (hardware run 32) the left Joy-Con works; the right
one, clipped on, is not seen. Its detect line (GPIO H6, behind UART-B's
TX pad G0 turned into an input) reads high, and so does G0 itself,
whatever the pad's pull (none, down, up), the charge enables (K3, CC3)
or a stray hand on the rail; a handshake forced on UART-B at 1 Mbaud
gets no byte. The code follows hekate's `joycon.c` for both sides, the
left rail works, and the pad registers read back as written.

To try, with options before coding: (a) check whether the right
Joy-Con wakes with hekate's own `uart_port=2` left out of
`hwboot/nxk.ini` (hekate then does not touch UART-B), (b) read the
pad and rail state hekate leaves (a dump from nxk before `joycon`
runs), (c) a different Joy-Con R or a second console to tell a unit
fault from a driver fault. Stick positions are also centred by
measurement, not from the unit's SPI calibration (0x603D).

## 3. `audio`: sound inside games is distorted or noisy

`audio tone` plays a clean 440 Hz or 1 kHz note on the Erista's speakers
(hardware run 34), but the effects of Starlane (a triangle-wave jingle,
a gem blip, a noise burst) were reported distorted or noisy, and the
tone played from the shell while a game runs was, before the ADMA ring
fix, not heard at all. Known: with the final ring the pump is never more
than one period late and the ring is never lapped (`audio` shows `max
step` and `laps` in its log line); the samples in the client ring are a
clean ramp at the service. Not known: whether what is left is the
signal (a triangle is harsh: try a sine, lower gains, the codec's
speaker DRC and Class D ratio, `RT5640_CLS_D_OUT` 0x8d), the supply
noise a busy GPU and display put on the amplifier (the hiss changed
pitch when the UI redrew, with the amplifier unmuted and idle), or the
stop / start of the stream around each sound (idle stop after 3 s, mute
and unmute pops, I2S reset at every start).

To try, with options before coding: (a) keep the stream running with
silence while a game has the audio, unmuting once (no start / stop pops),
(b) a sine and a gentler envelope for the effects, (c) measure on the
Switch whether the noise follows the GPU or the display (play a tone with
the GPU off, then `gpu bench`), (d) codec-side: speaker volume plus DAC
digital volume instead of full DAC and a quiet amplifier, DRC on.

Also open: no headphone path or jack detect (GPIO V6), no volume control,
one client at a time, no mipmaps for textures (Starlane's far grid is
faded to dark instead), and no alpha blending (Starlane's crash banner
is an opaque black band).

## 4. `audio`: 85 ms queued before the speakers

Done in code (emulator only; the timing needs the Erista, see below). The
service's DMA ring is 8 ADMA buffers; how many are kept filled and how
long they are is now a `Mode` per client. `OPEN` takes an optional word 1,
the frames the client wants ahead of the speakers at most (0 or absent:
the default, 8 x 512 frames = 85 ms, for games). Luna asks for 512
(`Stream::open_ahead`): 4 periods of 128 frames, about 8 to 11 ms, and its
`DEADLINE` is 384 frames. Slots past the filled ones hold silence, so a
late pump plays silence and not old sound again; the DMA restarts when a
client wants another mode than the one running (a tone or the melody plays
in whatever runs). The reply carries the period and depth granted.

Options weighed: (a) shorter periods for every client: changes game audio
while TASKS 3 is open and gives games a pump with no slack they do not
need; (b) the client asks at `OPEN` (chosen: games keep what they have,
the margin is per stream); (c) the client ring feeding the ADMA directly:
refused, a client's shared memory is not contiguous, DMA32 and uncached,
the ADMA wraps by its buffer count and the software gain and silence
padding would go.

Left to do on the Erista (needs the user's go-ahead; low volume): run
`audio stream 512 20000 10` (a quiet 440 Hz sine, 20 s, through a shallow
client ring), then `audio status` and read `late`, `fewest left` and
`longest gap`. `late` must be 0 and `fewest left` at least 1 with the
GPU and display busy (try `gpu bench` meanwhile); if not, raise
the queue Luna asks for (`AHEAD` in `player.rs`: 768 gives 6 periods of
128, 2048 gives 8 of 256). The emulator's DMA runs at 12.8 kHz, so its numbers prove the
logic, not the timing. Luna's own jitter buffer is still 50 ms (`TARGET`);
it can come down once the pump's margin is known. Games still have the
85 ms (their effects follow 43 ms of ring plus it): a follow-up once
TASKS 3 is settled.
